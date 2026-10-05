// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The few things said to JavaScript that have no typed binding worth its
//! weight: a property read, a method called by name, a constructor.
//!
//! WebCodecs is the reason this exists. Its `web-sys` bindings sit behind an
//! unstable flag, and the rule here is a stable toolchain; called by name,
//! `VideoEncoder` and `VideoFrame` need neither the flag nor a line of
//! JavaScript.

use js_sys::{Array, Function, Object, Promise, Reflect};
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;

/// `object[key]`, or `undefined`.
pub fn get(object: &JsValue, key: &str) -> JsValue {
    Reflect::get(object, &key.into()).unwrap_or(JsValue::UNDEFINED)
}

pub fn number(object: &JsValue, key: &str) -> f64 {
    get(object, key).as_f64().unwrap_or(0.0)
}

pub fn string(object: &JsValue, key: &str) -> String {
    get(object, key).as_string().unwrap_or_default()
}

/// A plain object from its entries.
pub fn object(entries: &[(&str, JsValue)]) -> Object {
    let out = Object::new();
    for (key, value) in entries {
        let _ = Reflect::set(&out, &(*key).into(), value);
    }
    out
}

/// `object.method(...args)`.
pub fn call(object: &JsValue, method: &str, args: &[JsValue]) -> Result<JsValue, JsValue> {
    let f: Function = get(object, method)
        .dyn_into()
        .map_err(|_| JsValue::from_str(&format!("{method} is not a function")))?;
    f.apply(object, &args.iter().collect::<Array>())
}

/// `new globalThis[name](...args)`.
pub fn construct(name: &str, args: &[JsValue]) -> Result<JsValue, JsValue> {
    let ctor: Function = get(&js_sys::global(), name)
        .dyn_into()
        .map_err(|_| JsValue::from_str(&format!("{name} is not available")))?;
    Reflect::construct(&ctor, &args.iter().collect::<Array>())
}

/// Whether `globalThis[name]` exists.
pub fn has(name: &str) -> bool {
    !get(&js_sys::global(), name).is_undefined()
}

/// The promise a call returned, awaited.
pub async fn settled(promise: JsValue) -> Result<JsValue, JsValue> {
    JsFuture::from(Promise::from(promise)).await
}

/// What an error says, whatever threw it.
pub fn text(error: impl Into<JsValue>) -> String {
    let value: JsValue = error.into();
    if let Some(e) = value.dyn_ref::<js_sys::Error>() {
        return String::from(e.message());
    }
    value.as_string().unwrap_or_else(|| format!("{value:?}"))
}

pub async fn sleep(ms: i32) {
    let promise = Promise::new(&mut |resolve, _| {
        let global = js_sys::global();
        if let Ok(set) = get(&global, "setTimeout").dyn_into::<Function>() {
            let _ = set.call2(&global, &resolve, &ms.into());
        }
    });
    let _ = JsFuture::from(promise).await;
}

/// The time in milliseconds, for measuring a span.
pub fn now() -> f64 {
    js_sys::Date::now()
}
