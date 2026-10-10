// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The three things the viewer asks of the desktop it runs on: receive
//! steering URLs, show a message when there is no terminal to print it in, and
//! put a line of text on the clipboard.
//!
//! The rest of the viewer sees three plain functions. The implementation that
//! talks to an operating system is the private `system` module, compiled on
//! macOS with the `application` feature; everywhere else URLs never arrive,
//! the message goes to stderr and the clipboard declines.
//!
//! **Exercised by hand only**, like the location service: nothing here can be
//! called from a test without a window server and a person. What the URLs
//! *mean* is tested in [`crate::steer`]; this module only carries the string.

use std::sync::mpsc::Receiver;

/// Starts listening for steering URLs and returns where they will arrive.
///
/// Call once, on the main thread, before the event loop starts: a URL that
/// *launched* the application is delivered as the loop begins, and a listener
/// installed later would miss it. The strings are whatever was sent — nothing
/// is trusted until [`crate::steer::parse_url`] has read it — and they are
/// only queued here: the callback runs inside the system's event dispatch, and
/// the camera is moved by the render loop when it drains the queue.
pub(crate) fn listen_for_urls() -> Receiver<String> {
    system::listen_for_urls()
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

    use std::sync::mpsc::{channel, Receiver, Sender};
    use std::sync::Mutex;

    use objc2::rc::Retained;
    use objc2::runtime::NSObject;
    use objc2::{define_class, msg_send, sel, AllocAnyThread, DefinedClass, MainThreadMarker};
    use objc2_app_kit::{
        NSAlert, NSApplication, NSApplicationActivationPolicy, NSPasteboard, NSPasteboardTypeString,
    };
    use objc2_foundation::{NSAppleEventDescriptor, NSAppleEventManager, NSString};

    /// `'GURL'`: both the class and the id of the "open this URL" Apple event.
    const GET_URL: u32 = u32::from_be_bytes(*b"GURL");
    /// `'----'`: the keyword of an Apple event's direct parameter — here, the
    /// URL itself.
    const DIRECT_OBJECT: u32 = u32::from_be_bytes(*b"----");

    define_class!(
        // SAFETY: `NSObject` has no subclassing requirements, and the class
        // does not implement `Drop`.
        #[unsafe(super(NSObject))]
        #[name = "TuileViewerUrlHandler"]
        #[ivars = Mutex<Sender<String>>]
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
                let Some(url) = url.and_then(|d| d.stringValue()) else {
                    return;
                };
                if let Ok(sender) = self.ivars().lock() {
                    // A closed channel means the window is gone; so is the
                    // point of steering it.
                    let _ = sender.send(url.to_string());
                }
            }
        }
    );

    pub(super) fn listen_for_urls() -> Receiver<String> {
        let (sender, receiver) = channel();
        let handler = UrlHandler::alloc().set_ivars(Mutex::new(sender));
        // SAFETY: `NSObject`'s `init` is the designated initialiser.
        let handler: Retained<UrlHandler> = unsafe { msg_send![super(handler), init] };
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

    pub(super) fn listen_for_urls() -> Receiver<String> {
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
