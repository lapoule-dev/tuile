// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The Android shell: the native shell, entered from the platform's native
//! activity instead of from `main`.
//!
//! An Android application is a shared library the system loads into a Java
//! process. `android-activity` (through winit's Android backend) runs the
//! library's `android_main` on a thread of its own, which becomes the
//! application's main thread here: the event loop runs on it and never on
//! the system's UI thread, so the UI thread is never held — no "application
//! not responding" whatever the session does.
//!
//! What this module adds to the native shell, before handing over to
//! [`super::main`]:
//!
//! - **directories**: the application's private files and cache directories,
//!   asked of the activity and given to the process as `HOME`,
//!   `XDG_CONFIG_HOME`, `XDG_STATE_HOME` and `XDG_CACHE_HOME`, so the support
//!   directory, the journal, the tile store and the HTTP cache land where
//!   every other platform's land, by the same code;
//! - **launch settings**: the launch intent's extras, the counterpart of a
//!   simulator's `SIMCTL_CHILD_` variables — the viewer's knobs only, never a
//!   credential (see [`super::android_signals::launch_settings`]);
//! - **the trust store**: the TLS verifier is the platform's, which on
//!   Android lives in Java and is reached through the activity's class
//!   loader;
//! - **the log**: tracing lines go to logcat, under the tag `tuile`;
//! - **the activity's services**, for `device` and `desktop`: memory signals,
//!   temperature, battery saver, the caption, alerts, the clipboard. Each is
//!   a method of `TuileActivity` (`android/java`), called from this thread;
//!   whatever touches a view is posted to the UI thread by the Java side.
//!
//! Every JNI call is best effort: a failure is logged once per method and
//! answered with the neutral value, never a panic in the frame loop.

#![allow(unsafe_code)]

use std::collections::HashSet;
use std::ffi::CString;
use std::sync::{Mutex, OnceLock};

use jni::objects::{JObject, JString, JValue};
use jni::{jni_sig, jni_str, Env, JavaVM};
use winit::platform::android::activity::AndroidApp;

use crate::embed::ViewerHost;

/// The activity the system started, for the event loop and every JNI call.
static APP: OnceLock<AndroidApp> = OnceLock::new();

/// The logcat tag every line of the viewer is written under.
const TAG: &str = "tuile";

/// Runs the application for `host` inside the activity `app`, from the
/// launch settings to the last flush of the tile store.
///
/// Called from the host's `android_main`. Like [`super::main`], it does not
/// return: the process ends with the session.
pub fn main(app: AndroidApp, host: impl ViewerHost) -> ! {
    std::panic::set_hook(Box::new(|panic| {
        log_line(LogPriority::Fatal, &format!("{panic}"));
    }));
    let _ = APP.set(app);
    prepare_the_process();
    super::main(host)
}

/// The event loop, bound to the activity.
pub(crate) fn event_loop() -> Result<winit::event_loop::EventLoop<()>, winit::error::EventLoopError>
{
    use winit::platform::android::EventLoopBuilderExtAndroid;
    let app = APP
        .get()
        .expect("the Android shell was entered through `main`")
        .clone();
    winit::event_loop::EventLoop::builder()
        .with_android_app(app)
        .build()
}

/// Directories, launch settings and the trust store, before any thread of
/// the session exists: the environment is set while this thread is the only
/// Rust thread there is.
fn prepare_the_process() {
    let files = call_string("filesDirectory").or_else(|| {
        APP.get()
            .and_then(AndroidApp::internal_data_path)
            .map(|p| p.display().to_string())
    });
    let cache = call_string("cacheDirectory");
    if let Some(files) = files.as_deref() {
        std::env::set_var("HOME", files);
        std::env::set_var("XDG_CONFIG_HOME", format!("{files}/config"));
        std::env::set_var("XDG_STATE_HOME", format!("{files}/state"));
    }
    if let Some(cache) = cache.as_deref().or(files.as_deref()) {
        std::env::set_var("XDG_CACHE_HOME", cache);
        std::env::set_var("TMPDIR", cache);
    }
    let settings =
        super::android_signals::launch_settings(&call_string("launchSettings").unwrap_or_default());
    for (key, value) in &settings.accepted {
        std::env::set_var(key, value);
    }
    if !settings.refused.is_empty() {
        log_line(
            LogPriority::Warn,
            &format!(
                "launch settings refused (not the viewer's, or a credential): {}",
                settings.refused.join(", ")
            ),
        );
    }
    trust_the_platform();
}

/// Hands the TLS verifier the JVM, the activity and its class loader: the
/// verifier asks Android's own trust store, through its Java half, which the
/// APK carries.
fn trust_the_platform() {
    let outcome = with_activity(|env, activity| {
        let loader = env
            .call_method(
                activity,
                jni_str!("getClassLoader"),
                jni_sig!("()Ljava/lang/ClassLoader;"),
                &[],
            )?
            .l()?;
        let loader = env.cast_local::<jni::objects::JClassLoader>(loader)?;
        rustls_platform_verifier::android::init_with_refs(
            env.get_java_vm()?,
            env.new_global_ref(activity)?,
            env.new_global_ref(loader)?,
        );
        Ok(())
    });
    if outcome.is_none() {
        log_line(
            LogPriority::Error,
            "the platform's trust store could not be reached: secure connections will fail",
        );
    }
}

/// Runs `f` with the JNI environment of this thread and the activity.
///
/// `None` when the call failed — a Java exception, a missing method — after
/// saying so once per call site name and clearing the exception, so the next
/// call starts clean.
fn with_activity<T>(f: impl FnOnce(&mut Env, &JObject) -> jni::errors::Result<T>) -> Option<T> {
    let app = APP.get()?;
    // SAFETY: the pointer is the process's JavaVM, valid for its lifetime,
    // as android-activity documents.
    let vm = unsafe { JavaVM::from_raw(app.vm_as_ptr().cast()) };
    let result = vm.attach_current_thread(|env| -> jni::errors::Result<T> {
        // SAFETY: the activity's global reference, held by the native
        // activity for as long as the activity exists; never deleted here.
        let activity = unsafe { JObject::from_raw(env, app.activity_as_ptr().cast()) };
        let out = f(env, &activity);
        if out.is_err() && env.exception_check() {
            env.exception_describe();
            env.exception_clear();
        }
        out
    });
    match result {
        Ok(value) => Some(value),
        Err(error) => {
            say_once(&format!("{error}"));
            None
        }
    }
}

/// Logs a failure once: a frame loop asking every second must not flood
/// logcat with the same line.
fn say_once(what: &str) {
    static SAID: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    let said = SAID.get_or_init(Default::default);
    if said.lock().is_ok_and(|mut s| s.insert(what.to_owned())) {
        log_line(LogPriority::Warn, &format!("activity call failed: {what}"));
    }
}

fn call_string(method: &'static str) -> Option<String> {
    with_activity(|env, activity| {
        let name = jni::strings::JNIString::from(method);
        let value = env
            .call_method(activity, &name, jni_sig!("()Ljava/lang/String;"), &[])?
            .l()?;
        if value.is_null() {
            return Ok(None);
        }
        let value = env.cast_local::<JString>(value)?;
        value.try_to_string(env).map(Some)
    })
    .flatten()
}

/// The activity's trim signal: how many times the system said it is short
/// of memory, and the last level, packed. See `android_signals::unpack_trim`.
pub(crate) fn trim_signal() -> i64 {
    with_activity(|env, activity| {
        env.call_method(activity, jni_str!("trimSignal"), jni_sig!("()J"), &[])?
            .j()
    })
    .unwrap_or(0)
}

/// What the system's bars and the cutout cover of the window, packed; `-1`
/// when unknown. See `android_signals::safe_area_of_insets`.
pub(crate) fn safe_insets() -> i64 {
    with_activity(|env, activity| {
        env.call_method(activity, jni_str!("safeInsets"), jni_sig!("()J"), &[])?
            .j()
    })
    .unwrap_or(-1)
}

/// `PowerManager`'s thermal status, 0 when unknown.
pub(crate) fn thermal_status() -> i32 {
    with_activity(|env, activity| {
        env.call_method(activity, jni_str!("thermalStatus"), jni_sig!("()I"), &[])?
            .i()
    })
    .unwrap_or(0)
}

/// Whether the battery saver is on.
pub(crate) fn power_save() -> bool {
    with_activity(|env, activity| {
        env.call_method(activity, jni_str!("powerSave"), jni_sig!("()Z"), &[])?
            .z()
    })
    .unwrap_or(false)
}

/// The caption over the bottom of the view.
pub(crate) fn caption(text: &str) {
    with_activity(|env, activity| {
        let text = env.new_string(text)?;
        env.call_method(
            activity,
            jni_str!("caption"),
            jni_sig!("(Ljava/lang/String;)V"),
            &[JValue::Object(&text)],
        )?;
        Ok(())
    });
}

/// An alert over the view; whether it could be shown now.
pub(crate) fn alert(title: &str, text: &str) -> bool {
    with_activity(|env, activity| {
        let title = env.new_string(title)?;
        let text = env.new_string(text)?;
        env.call_method(
            activity,
            jni_str!("alert"),
            jni_sig!("(Ljava/lang/String;Ljava/lang/String;)Z"),
            &[JValue::Object(&title), JValue::Object(&text)],
        )?
        .z()
    })
    .unwrap_or(false)
}

/// Puts `text` on the clipboard.
pub(crate) fn copy(text: &str) -> bool {
    with_activity(|env, activity| {
        let text = env.new_string(text)?;
        env.call_method(
            activity,
            jni_str!("copy"),
            jni_sig!("(Ljava/lang/String;)Z"),
            &[JValue::Object(&text)],
        )?
        .z()
    })
    .unwrap_or(false)
}

/// The text on the clipboard.
pub(crate) fn paste() -> Option<String> {
    call_string("paste")
}

/// What a view or a share brought since the last call — a configuration
/// file or an import link — as the request the loop carries out.
pub(crate) fn take_import() -> Option<crate::steer::Request> {
    crate::native::android_signals::import_request(&call_string("takeImport")?)
}

/// Seals `value` under `name` in the activity's Keystore-backed store.
pub(crate) fn seal_secret(name: &str, value: &str) -> bool {
    with_activity(|env, activity| {
        let name = env.new_string(name)?;
        let value = env.new_string(value)?;
        env.call_method(
            activity,
            jni_str!("sealSecret"),
            jni_sig!("(Ljava/lang/String;Ljava/lang/String;)Z"),
            &[JValue::Object(&name), JValue::Object(&value)],
        )?
        .z()
    })
    .unwrap_or(false)
}

/// The value sealed under `name`, if there is one.
pub(crate) fn open_secret(name: &str) -> Option<String> {
    with_activity(|env, activity| {
        let name = env.new_string(name)?;
        let value = env
            .call_method(
                activity,
                jni_str!("openSecret"),
                jni_sig!("(Ljava/lang/String;)Ljava/lang/String;"),
                &[JValue::Object(&name)],
            )?
            .l()?;
        if value.is_null() {
            return Ok(None);
        }
        let value = env.cast_local::<JString>(value)?;
        value.try_to_string(env).map(Some)
    })
    .flatten()
}

/// Logcat's priorities, as `<android/log.h>` numbers them.
#[derive(Clone, Copy)]
pub(crate) enum LogPriority {
    Debug = 3,
    Info = 4,
    Warn = 5,
    Error = 6,
    Fatal = 7,
}

/// One line to logcat.
pub(crate) fn log_line(priority: LogPriority, text: &str) {
    let tag = CString::new(TAG).unwrap_or_default();
    let text = CString::new(text.replace('\0', " ")).unwrap_or_default();
    // SAFETY: two valid, NUL-terminated strings that outlive the call; the
    // function copies them.
    unsafe {
        android_log_sys::__android_log_write(priority as _, tag.as_ptr(), text.as_ptr());
    }
}

/// Tracing's output, a line at a time, into logcat — at the priority of the
/// level the line starts with, since the formatter writes it first.
pub(crate) struct Logcat {
    line: Vec<u8>,
}

impl Logcat {
    pub(crate) fn new() -> Self {
        Self { line: Vec::new() }
    }

    fn flush_line(&mut self) {
        if self.line.is_empty() {
            return;
        }
        let text = String::from_utf8_lossy(&self.line).into_owned();
        self.line.clear();
        let level = text.trim_start();
        let priority = if level.starts_with("ERROR") {
            LogPriority::Error
        } else if level.starts_with("WARN") {
            LogPriority::Warn
        } else if level.starts_with("INFO") {
            LogPriority::Info
        } else {
            LogPriority::Debug
        };
        log_line(priority, text.trim_end());
    }
}

impl std::io::Write for Logcat {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        for &byte in buf {
            if byte == b'\n' {
                self.flush_line();
            } else {
                self.line.push(byte);
            }
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.flush_line();
        Ok(())
    }
}

impl Drop for Logcat {
    fn drop(&mut self) {
        self.flush_line();
    }
}
