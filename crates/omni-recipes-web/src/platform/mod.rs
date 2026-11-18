//! The browser: small wrappers so the rest of the app reads as plain Rust.

pub mod device;
pub mod idb;
pub mod net;

use omni_api::common::Ms;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;

pub fn window() -> web_sys::Window {
    #[allow(clippy::expect_used)]
    web_sys::window().expect("the app runs in a window")
}

pub fn document() -> web_sys::Document {
    #[allow(clippy::expect_used)]
    window().document().expect("the window has a document")
}

pub fn now_ms() -> Ms {
    js_sys::Date::now() as Ms
}

/// `prefix` plus `len` random lowercase letters and digits.
pub fn random_id(prefix: &str, len: usize) -> String {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut id = String::from(prefix);
    for _ in 0..len {
        let at = (js_sys::Math::random() * ALPHABET.len() as f64) as usize;
        id.push(char::from(ALPHABET[at.min(ALPHABET.len() - 1)]));
    }
    id
}

pub async fn sleep(ms: u32) {
    let promise = js_sys::Promise::new(&mut |resolve, _| {
        let _ = window().set_timeout_with_callback_and_timeout_and_arguments_0(
            &resolve,
            i32::try_from(ms).unwrap_or(i32::MAX),
        );
    });
    let _ = JsFuture::from(promise).await;
}

/// `object[key]`, undefined when missing.
pub fn get(object: &JsValue, key: &str) -> JsValue {
    js_sys::Reflect::get(object, &JsValue::from_str(key)).unwrap_or(JsValue::UNDEFINED)
}

/// `object.method(...args)`; an error when the method does not exist.
pub fn call(object: &JsValue, method: &str, args: &[JsValue]) -> Result<JsValue, JsValue> {
    let function: js_sys::Function = get(object, method).dyn_into()?;
    function.apply(object, &args.iter().collect::<js_sys::Array>())
}

/// Awaits a value that should be a promise.
pub async fn settle(value: Result<JsValue, JsValue>) -> Result<JsValue, JsValue> {
    JsFuture::from(value?.dyn_into::<js_sys::Promise>()?).await
}

/// Adds a listener that lives as long as the page.
pub fn listen(
    target: &web_sys::EventTarget,
    name: &str,
    handler: impl FnMut(web_sys::Event) + 'static,
) {
    let closure = Closure::<dyn FnMut(web_sys::Event)>::new(handler);
    let _ = target.add_event_listener_with_callback(name, closure.as_ref().unchecked_ref());
    closure.forget();
}

pub fn visible() -> bool {
    document().visibility_state() == web_sys::VisibilityState::Visible
}
