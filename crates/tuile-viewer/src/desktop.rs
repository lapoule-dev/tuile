// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! What the viewer asks of the desktop it runs on: receive steering — URLs,
//! and a script's questions and commands — show a message when there is no
//! terminal to print it in, and put a line of text on the clipboard.
//!
//! The rest of the viewer sees three plain functions. The implementation that
//! talks to an operating system is the private `system` module, compiled on
//! macOS with the `application` feature; everywhere else nothing ever arrives,
//! the message goes to stderr and the clipboard declines.
//!
//! **Exercised by hand only**, like the location service: nothing here can be
//! called from a test without a window server and a person. What a URL
//! *means* is tested in [`crate::steer`], what a property *answers* in
//! [`crate::snapshot`]; this module only carries strings and numbers across.
//! `macos/scripting-check.sh` runs the real thing against an installed bundle.

use std::sync::mpsc::Receiver;

use crate::steer::Request;

/// Starts listening for steering and returns where it will arrive.
///
/// Call once, on the main thread, before the event loop starts: a URL that
/// *launched* the application is delivered as the loop begins, and a listener
/// installed later would miss it. Nothing that arrives is trusted — a URL is
/// only a string until [`crate::steer::parse_url`] has read it, a script's
/// numbers are held to the flags' bounds before they become a command — and
/// everything is only queued here: the handlers run inside the system's event
/// dispatch, and the camera is moved by the render loop when it drains the
/// queue. A script's *questions* never reach the loop at all; they are
/// answered from [`crate::snapshot::current`].
pub(crate) fn listen() -> Receiver<Request> {
    system::listen()
}

/// Shows a message to someone who may have no terminal, and waits for them to
/// dismiss it.
pub(crate) fn alert(title: &str, text: &str) {
    system::alert(title, text);
}

/// Puts `text` on the clipboard; `false` when there is no clipboard to put it
/// on.
pub(crate) fn copy(text: &str) -> bool {
    system::copy(text)
}

#[cfg(all(target_os = "macos", feature = "application"))]
mod system {
    #![allow(unsafe_code)]

    use std::ffi::CString;
    use std::sync::mpsc::{channel, Receiver, Sender};
    use std::sync::Mutex;

    use objc2::ffi::class_addMethod;
    use objc2::rc::Retained;
    use objc2::runtime::{AnyClass, AnyObject, Imp, NSObject, Sel};
    use objc2::{define_class, msg_send, sel, AllocAnyThread, ClassType, MainThreadMarker};
    use objc2_app_kit::{
        NSAlert, NSApplication, NSApplicationActivationPolicy, NSPasteboard, NSPasteboardTypeString,
    };
    use objc2_foundation::{
        NSAppleEventDescriptor, NSAppleEventManager, NSNumber, NSScriptCommand, NSString,
    };

    use crate::snapshot::{self, Value, PROPERTIES};
    use crate::steer::{Command, Goto, Request};

    /// `'GURL'`: both the class and the id of the "open this URL" Apple event.
    const GET_URL: u32 = u32::from_be_bytes(*b"GURL");
    /// `'----'`: the keyword of an Apple event's direct parameter — here, the
    /// URL itself.
    const DIRECT_OBJECT: u32 = u32::from_be_bytes(*b"----");
    /// The event ids of the dictionary's three commands, as `Tuile.sdef`
    /// spells them after the suite's `Tuil`.
    const GO_TO: u32 = u32::from_be_bytes(*b"goto");
    const NORTH_UP: u32 = u32::from_be_bytes(*b"nrth");
    const HERE: u32 = u32::from_be_bytes(*b"here");
    /// The system's "a parameter is wrong" error number.
    const PARAMETER_ERROR: isize = -50;

    /// Where every handler below leaves what it received. One queue, because
    /// the render loop has one place to look.
    static OUTBOX: Mutex<Option<Sender<Request>>> = Mutex::new(None);

    fn send(request: Request) {
        if let Ok(outbox) = OUTBOX.lock() {
            if let Some(sender) = outbox.as_ref() {
                // A closed channel means the window is gone; so is the point
                // of steering it.
                let _ = sender.send(request);
            }
        }
    }

    define_class!(
        // SAFETY: `NSObject` has no subclassing requirements, and the class
        // does not implement `Drop`.
        #[unsafe(super(NSObject))]
        #[name = "TuileViewerUrlHandler"]
        struct UrlHandler;

        impl UrlHandler {
            /// The signature the event manager calls: the event, and a reply
            /// this handler leaves empty.
            #[unsafe(method(handleURL:withReply:))]
            fn handle(&self, event: &NSAppleEventDescriptor, _reply: &NSAppleEventDescriptor) {
                // SAFETY: `paramDescriptorForKeyword:` takes a four-character
                // code and returns a descriptor or nil.
                let url: Option<Retained<NSAppleEventDescriptor>> =
                    unsafe { msg_send![event, paramDescriptorForKeyword: DIRECT_OBJECT] };
                if let Some(url) = url.and_then(|d| d.stringValue()) {
                    send(Request::Url(url.to_string()));
                }
            }
        }
    );

    define_class!(
        // SAFETY: `NSScriptCommand` is meant to be subclassed, with exactly
        // this method overridden; the class does not implement `Drop`.
        //
        // One class for the three commands of the dictionary: the scripting
        // runtime instantiates it by name and the event's own code says which
        // command it is.
        #[unsafe(super(NSScriptCommand))]
        #[name = "TuileViewerScriptCommand"]
        struct ScriptCommand;

        impl ScriptCommand {
            #[unsafe(method_id(performDefaultImplementation))]
            fn perform(&self) -> Option<Retained<AnyObject>> {
                let description = self.commandDescription();
                // SAFETY: a plain property read returning a four-character
                // code.
                let code: u32 = unsafe { msg_send![&*description, appleEventCode] };
                let command = match code {
                    NORTH_UP => Ok(Command::NorthUp),
                    HERE => Ok(Command::Here),
                    GO_TO => self.goto().map(Command::Goto),
                    _ => Err("not a command of this application".to_owned()),
                };
                match command {
                    Ok(command) => send(Request::Do(command)),
                    // The script is told, in its own terms: an error number
                    // and the sentence the flags would have printed.
                    Err(why) => {
                        self.setScriptErrorNumber(PARAMETER_ERROR);
                        self.setScriptErrorString(Some(&NSString::from_str(&why)));
                    }
                }
                None
            }
        }
    );

    impl ScriptCommand {
        /// The `go to` this event carries, its numbers held to the flags'
        /// bounds. The argument names are the dictionary's `cocoa key`s.
        fn goto(&self) -> Result<Goto, String> {
            let arguments = self.evaluatedArguments();
            let mut given = Vec::new();
            for name in ["lon", "lat", "altitude", "heading", "pitch"] {
                let value = arguments
                    .as_ref()
                    .and_then(|a| a.objectForKey(&NSString::from_str(name)));
                if let Some(value) = value {
                    // SAFETY: the dictionary types these parameters `real`,
                    // so the runtime hands over numbers.
                    let number: f64 = unsafe { msg_send![&*value, doubleValue] };
                    given.push((name, number));
                }
            }
            Goto::from_numbers(&given)
        }
    }

    /// Answers one property of the application object: the selector *is* the
    /// key. Every answer is an object — a number or a string — or nil, which a
    /// script reads as `missing value`.
    extern "C-unwind" fn get(_app: &AnyObject, key: Sel) -> *mut AnyObject {
        let answer = key
            .name()
            .to_str()
            .ok()
            .zip(snapshot::current())
            .and_then(|(key, view)| snapshot::property(&view, key));
        match answer {
            Some(Value::Real(x)) => Retained::autorelease_ptr(NSNumber::new_f64(x)).cast(),
            Some(Value::Integer(n)) => Retained::autorelease_ptr(NSNumber::new_i64(n)).cast(),
            Some(Value::Flag(on)) => Retained::autorelease_ptr(NSNumber::new_bool(on)).cast(),
            Some(Value::Text(text)) => Retained::autorelease_ptr(NSString::from_str(&text)).cast(),
            Some(Value::Withheld | Value::Missing) | None => std::ptr::null_mut(),
        }
    }

    /// Takes a script's `set wireframe to …` / `set frozen to …`: the selector
    /// says which, the value is a number, and what comes of it is a command on
    /// the queue like any other.
    extern "C-unwind" fn set(_app: &AnyObject, setter: Sel, value: *mut AnyObject) {
        if value.is_null() {
            return;
        }
        if setter.name().to_bytes() == b"setImagery:" {
            // SAFETY: the dictionary types this property `text`, so the
            // runtime hands over a string; `description` is defined on every
            // object and returns one whatever it was handed.
            let name: Retained<NSString> = unsafe { msg_send![value, description] };
            // A name and nothing else: which layer it is, if any, is decided
            // by the render loop against the host's list.
            send(Request::Imagery(name.to_string()));
            return;
        }
        // SAFETY: the dictionary types both properties `boolean`, so the
        // runtime hands over a number; `boolValue` is a plain read.
        let on: bool = unsafe { msg_send![value, boolValue] };
        match setter.name().to_bytes() {
            b"setWireframe:" => send(Request::Do(Command::Wireframe(on))),
            b"setFrozen:" => send(Request::Do(Command::Freeze(on))),
            _ => {}
        }
    }

    /// Gives the application object the dictionary's properties.
    ///
    /// The scripting runtime reads a property of `application` by key-value
    /// coding on the shared application object, which means: by calling a
    /// method named like the key. The windowing layer owns that object's class
    /// and its delegate, so the methods are added to the class at run time —
    /// what a category does in the system's own language — one per key of
    /// [`PROPERTIES`], all pointing at the two functions above.
    fn add_the_properties() {
        let class = (NSApplication::class() as *const AnyClass).cast_mut();
        for (key, _, writable) in PROPERTIES {
            let Ok(name) = CString::new(key) else {
                continue;
            };
            // SAFETY: `get` has the signature its type string states — an
            // object returned, from `self` and `_cmd` — and the class is the
            // live `NSApplication` class.
            unsafe {
                let imp: Imp = std::mem::transmute::<
                    extern "C-unwind" fn(&AnyObject, Sel) -> *mut AnyObject,
                    Imp,
                >(get);
                class_addMethod(class, Sel::register(&name), imp, c"@@:".as_ptr());
            }
            if !writable {
                continue;
            }
            let mut capital = key.to_owned();
            capital[..1].make_ascii_uppercase();
            let Ok(name) = CString::new(format!("set{capital}:")) else {
                continue;
            };
            // SAFETY: as above, for a method taking one object and returning
            // nothing.
            unsafe {
                let imp: Imp = std::mem::transmute::<
                    extern "C-unwind" fn(&AnyObject, Sel, *mut AnyObject),
                    Imp,
                >(set);
                class_addMethod(class, Sel::register(&name), imp, c"v@:@".as_ptr());
            }
        }
    }

    pub(super) fn listen() -> Receiver<Request> {
        let (sender, receiver) = channel();
        if let Ok(mut outbox) = OUTBOX.lock() {
            *outbox = Some(sender);
        }
        let handler = UrlHandler::alloc();
        // SAFETY: `NSObject`'s `init` is the designated initialiser.
        let handler: Retained<UrlHandler> = unsafe { msg_send![handler, init] };
        let manager = NSAppleEventManager::sharedAppleEventManager();
        // SAFETY: the selector exists on the handler with the signature the
        // manager calls, and the two codes are plain `u32`s.
        let _: () = unsafe {
            msg_send![
                &*manager,
                setEventHandler: &*handler,
                andSelector: sel!(handleURL:withReply:),
                forEventClass: GET_URL,
                andEventID: GET_URL,
            ]
        };
        // The manager does not keep its handlers alive, and this one is wanted
        // for as long as the process runs.
        std::mem::forget(handler);
        // The scripting runtime finds the command class by the name the
        // dictionary gives; it has to exist before the first script asks.
        let _ = ScriptCommand::class();
        add_the_properties();
        receiver
    }

    pub(super) fn alert(title: &str, text: &str) {
        let Some(mtm) = MainThreadMarker::new() else {
            eprintln!("{title}\n\n{text}");
            return;
        };
        // An alert from a process that is not yet an application appears
        // behind everything, or not at all: become one, and come forward.
        let app = NSApplication::sharedApplication(mtm);
        app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
        #[allow(deprecated)]
        app.activateIgnoringOtherApps(true);
        let alert = NSAlert::new(mtm);
        alert.setMessageText(&NSString::from_str(title));
        alert.setInformativeText(&NSString::from_str(text));
        alert.runModal();
    }

    pub(super) fn copy(text: &str) -> bool {
        let board = NSPasteboard::generalPasteboard();
        board.clearContents();
        // SAFETY: the string type is a constant the framework exports.
        board.setString_forType(&NSString::from_str(text), unsafe { NSPasteboardTypeString })
    }
}

#[cfg(not(all(target_os = "macos", feature = "application")))]
mod system {
    use std::sync::mpsc::{channel, Receiver};

    use crate::steer::Request;

    pub(super) fn listen() -> Receiver<Request> {
        // The sender is dropped here: the queue is simply always empty.
        channel().1
    }

    pub(super) fn alert(title: &str, text: &str) {
        eprintln!("{title}\n\n{text}");
    }

    pub(super) fn copy(_text: &str) -> bool {
        false
    }
}
