//! The page shell. The document never scrolls (so its header and footer
//! cannot rubber-band); each screen has one scrolling area between them.
//! This module finds that area, scrolls it, and keeps the shell above the
//! on-screen keyboard.

use wasm_bindgen::{JsCast, JsValue};

use super::{call, document, get, listen, window};

/// A keyboard is at least this tall; smaller differences are browser bars.
const KEYBOARD_MIN_PX: f64 = 80.0;

/// The current screen's scrolling area.
pub fn scroller() -> Option<web_sys::Element> {
    document().query_selector(".scroller").ok().flatten()
}

pub fn scroll_top() -> f64 {
    scroller().map_or(0.0, |area| f64::from(area.scroll_top()))
}

pub fn set_scroll_top(top: f64) {
    if let Some(area) = scroller() {
        #[allow(clippy::cast_possible_truncation)]
        area.set_scroll_top(top.round() as i32);
    }
}

fn scroll_to_top() {
    let Some(area) = scroller() else { return };
    let options = js_sys::Object::new();
    let _ = js_sys::Reflect::set(&options, &"top".into(), &JsValue::from_f64(0.0));
    let _ = js_sys::Reflect::set(&options, &"behavior".into(), &"smooth".into());
    if call(&area, "scrollTo", &[options.into()]).is_err() {
        area.set_scroll_top(0);
    }
}

fn number(object: &JsValue, key: &str) -> f64 {
    get(object, key).as_f64().unwrap_or(0.0)
}

/// How much of the bottom of the window the keyboard covers.
fn keyboard_inset(viewport: &JsValue) -> f64 {
    let window_height = window()
        .inner_height()
        .ok()
        .and_then(|height| height.as_f64())
        .unwrap_or(0.0);
    let covered = window_height - number(viewport, "height") - number(viewport, "offsetTop");
    if covered >= KEYBOARD_MIN_PX {
        covered
    } else {
        0.0
    }
}

fn follow_keyboard(viewport: &JsValue) {
    let Some(root) = document().document_element() else {
        return;
    };
    let inset = keyboard_inset(viewport);
    let style = get(&root, "style");
    let _ = call(
        &style,
        "setProperty",
        &["--kb".into(), format!("{inset}px").into()],
    );
    let _ = root.class_list().toggle_with_force("kb", inset > 0.0);
    // iOS pans a document that cannot scroll to reveal the field and leaves
    // it panned. The shell has been resized instead, so put the page back.
    window().scroll_to_with_x_and_y(0.0, 0.0);
    if inset > 0.0
        && let Some(field) = document().active_element()
        && field.closest(".scroller, .sheet").ok().flatten().is_some()
    {
        let options = js_sys::Object::new();
        let _ = js_sys::Reflect::set(&options, &"block".into(), &"nearest".into());
        let _ = call(&field, "scrollIntoView", &[options.into()]);
    }
}

/// Starts the shell's behaviour for the life of the page: a tap on a header
/// scrolls the screen to its top (the status-bar tap needs a scrolling
/// document, which this app does not have), and the layout follows the
/// keyboard.
pub fn install() {
    listen(&document(), "click", |event| {
        let Some(target) = event
            .target()
            .and_then(|target| target.dyn_into::<web_sys::Element>().ok())
        else {
            return;
        };
        let in_header = target.closest(".bar").ok().flatten().is_some();
        let on_control = target
            .closest("button, a, input, select, textarea, label")
            .ok()
            .flatten()
            .is_some();
        if in_header && !on_control {
            scroll_to_top();
        }
    });
    retry_failed_images();
    let viewport = get(&window(), "visualViewport");
    let Ok(target) = viewport.clone().dyn_into::<web_sys::EventTarget>() else {
        return;
    };
    for name in ["resize", "scroll"] {
        let viewport = viewport.clone();
        listen(&target, name, move |_| follow_keyboard(&viewport));
    }
}

/// How often a picture that failed to load is asked for again.
const IMAGE_RETRIES: u32 = 3;

/// Asks again for a recipe picture that failed to load. The server refuses
/// requests over its limit, and a long library can ask for more covers at
/// once than that; the service worker paces them, but a page it does not
/// control yet (the first visit) is on its own.
fn retry_failed_images() {
    use wasm_bindgen::closure::Closure;
    let handler = Closure::<dyn FnMut(web_sys::Event)>::new(|event: web_sys::Event| {
        let Some(image) = event
            .target()
            .and_then(|target| target.dyn_into::<web_sys::HtmlImageElement>().ok())
        else {
            return;
        };
        let src = image.get_attribute("src").unwrap_or_default();
        if !(src.starts_with("/img/") || src.starts_with("/src/")) {
            return;
        }
        let tried = image
            .get_attribute("data-retry")
            .and_then(|count| count.parse::<u32>().ok())
            .unwrap_or(0);
        if tried >= IMAGE_RETRIES {
            return;
        }
        let _ = image.set_attribute("data-retry", &(tried + 1).to_string());
        leptos::task::spawn_local(async move {
            super::sleep(700 * (tried + 1)).await;
            // Only if the page still shows this picture.
            if image.is_connected() && image.get_attribute("src").as_deref() == Some(&src) {
                image.set_src(&src);
            }
        });
    });
    // `error` does not bubble: it is caught on the way down.
    let _ = document().add_event_listener_with_callback_and_bool(
        "error",
        handler.as_ref().unchecked_ref(),
        true,
    );
    handler.forget();
}
