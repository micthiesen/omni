//! Phone capabilities: the service worker, the screen wake lock, the timer
//! sound, and shrinking photos before upload.

use std::cell::{Cell, RefCell};

use leptos::prelude::*;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;

use super::{call, document, get, listen, settle, window};

thread_local! {
    static REGISTRATION: RefCell<Option<web_sys::ServiceWorkerRegistration>> = const { RefCell::new(None) };
    static RELOAD_ON_TAKEOVER: Cell<bool> = const { Cell::new(false) };
    static WORKER_RUNNING: Cell<bool> = const { Cell::new(false) };
    static WAKE_LOCK: RefCell<Option<JsValue>> = const { RefCell::new(None) };
    static AUDIO: RefCell<Option<web_sys::AudioContext>> = const { RefCell::new(None) };
}

fn service_workers() -> Option<web_sys::ServiceWorkerContainer> {
    let navigator = window().navigator();
    (!get(&navigator, "serviceWorker").is_undefined()).then(|| navigator.service_worker())
}

/// Registers `/sw.js`. A new build that finished installing while the app was
/// closed takes over now, at launch; one that arrives while the app is open
/// waits and raises `update_available`. `on_control` runs when the first
/// worker takes control of a page that loaded without one.
pub fn register_service_worker(update_available: RwSignal<bool>, on_control: impl Fn() + 'static) {
    let Some(container) = service_workers() else {
        return;
    };
    listen(&container, "controllerchange", move |_| {
        if RELOAD_ON_TAKEOVER.replace(false) {
            let _ = window().location().reload();
        } else {
            on_control();
        }
    });
    let had_controller = container.controller().is_some();
    WORKER_RUNNING.set(had_controller);
    leptos::task::spawn_local(async move {
        let Ok(registration) = JsFuture::from(container.register("/sw.js")).await else {
            return;
        };
        let Ok(registration) = registration.dyn_into::<web_sys::ServiceWorkerRegistration>() else {
            return;
        };
        REGISTRATION.with_borrow_mut(|slot| *slot = Some(registration.clone()));
        if registration.waiting().is_some() && had_controller {
            apply_update();
            return;
        }
        let watched = registration.clone();
        listen(&registration, "updatefound", move |_| {
            let Some(installing) = watched.installing() else {
                return;
            };
            let worker = installing.clone();
            listen(&installing, "statechange", move |_| match worker.state() {
                // WebKit reports a controller while the very first worker is
                // still installing, so "is this an update" is remembered from
                // before it, not read from the container here.
                web_sys::ServiceWorkerState::Installed if WORKER_RUNNING.get() => {
                    update_available.set(true);
                }
                web_sys::ServiceWorkerState::Activated => WORKER_RUNNING.set(true),
                _ => {}
            });
        });
    });
}

/// Asks the browser to look for a new build.
pub fn check_for_update() {
    REGISTRATION.with_borrow(|slot| {
        if let Some(registration) = slot {
            // The promise only reports whether the check ran.
            let _ = registration.update();
        }
    });
}

/// Activates the waiting build and reloads into it.
pub fn apply_update() {
    let waiting = REGISTRATION.with_borrow(|slot| slot.as_ref().and_then(|r| r.waiting()));
    let Some(waiting) = waiting else {
        let _ = window().location().reload();
        return;
    };
    RELOAD_ON_TAKEOVER.set(true);
    let message = js_sys::Object::new();
    let _ = js_sys::Reflect::set(&message, &"type".into(), &"skip-waiting".into());
    let _ = waiting.post_message(&message);
}

/// Has the service worker fetch and keep the images it does not have yet.
pub fn cache_images(urls: &[String]) {
    let Some(controller) = service_workers().and_then(|c| c.controller()) else {
        return;
    };
    let list: js_sys::Array = urls.iter().map(|url| JsValue::from_str(url)).collect();
    let message = js_sys::Object::new();
    let _ = js_sys::Reflect::set(&message, &"type".into(), &"cache-images".into());
    let _ = js_sys::Reflect::set(&message, &"urls".into(), &list);
    let _ = controller.post_message(&message);
}

/// Has the service worker delete cached `/img/` files whose digest is not in
/// `digests`.
pub fn prune_images(digests: &[String]) {
    let Some(controller) = service_workers().and_then(|c| c.controller()) else {
        return;
    };
    let list: js_sys::Array = digests
        .iter()
        .map(|digest| JsValue::from_str(digest))
        .collect();
    let message = js_sys::Object::new();
    let _ = js_sys::Reflect::set(&message, &"type".into(), &"prune-images".into());
    let _ = js_sys::Reflect::set(&message, &"digests".into(), &list);
    let _ = controller.post_message(&message);
}

/// Asks the browser not to evict the library under storage pressure.
pub fn persist_storage() {
    let storage = get(&window().navigator(), "storage");
    if !storage.is_undefined() {
        // Granted or not, the app works the same; the answer is not needed.
        let _ = call(&storage, "persist", &[]);
    }
}

/// Requests the screen wake lock; false when the browser refuses or has none.
pub async fn request_wake_lock() -> bool {
    let wake_lock = get(&window().navigator(), "wakeLock");
    if wake_lock.is_undefined() || !super::visible() {
        return false;
    }
    match settle(call(&wake_lock, "request", &["screen".into()])).await {
        Ok(sentinel) => {
            WAKE_LOCK.with_borrow_mut(|slot| *slot = Some(sentinel));
            true
        }
        Err(_) => false,
    }
}

pub fn release_wake_lock() {
    if let Some(sentinel) = WAKE_LOCK.with_borrow_mut(Option::take) {
        let _ = call(&sentinel, "release", &[]);
    }
}

/// Creates or resumes the audio context. iOS only allows this inside a tap,
/// so it is called when a timer is started, long before it rings.
pub fn prime_audio() {
    AUDIO.with_borrow_mut(|slot| {
        if slot.is_none() {
            *slot = web_sys::AudioContext::new().ok();
        }
        if let Some(context) = slot {
            let _ = context.resume();
        }
    });
}

/// Three short tones, and a vibration where the phone supports one.
pub fn ring() {
    AUDIO.with_borrow(|slot| {
        let Some(context) = slot else { return };
        // iOS suspends the context while the app is in the background.
        let _ = context.resume();
        let start = context.current_time();
        for n in 0..3 {
            let at = start + f64::from(n) * 0.45;
            let (Ok(tone), Ok(gain)) = (context.create_oscillator(), context.create_gain()) else {
                return;
            };
            tone.frequency().set_value(880.0);
            let _ = gain.gain().set_value_at_time(0.0001, at);
            let _ = gain
                .gain()
                .exponential_ramp_to_value_at_time(0.5, at + 0.02);
            let _ = gain
                .gain()
                .exponential_ramp_to_value_at_time(0.0001, at + 0.35);
            let _ = tone.connect_with_audio_node(&gain);
            let _ = gain.connect_with_audio_node(&context.destination());
            let _ = tone.start_with_when(at);
            let _ = tone.stop_with_when(at + 0.4);
        }
    });
    let navigator = window().navigator();
    if !get(&navigator, "vibrate").is_undefined() {
        let pattern: js_sys::Array = [300, 150, 300, 150, 300]
            .into_iter()
            .map(JsValue::from)
            .collect();
        let _ = call(&navigator, "vibrate", &[pattern.into()]);
    }
}

/// Decodes a photo and re-encodes it as a JPEG no longer than `max_edge` on
/// its long side.
pub async fn downscale_jpeg(
    file: &web_sys::File,
    max_edge: u32,
) -> Result<js_sys::Uint8Array, JsValue> {
    let url = web_sys::Url::create_object_url_with_blob(file)?;
    let image = web_sys::HtmlImageElement::new()?;
    image.set_src(&url);
    let decoded = JsFuture::from(image.decode()).await;
    let result = async {
        decoded?;
        let (width, height) = (image.natural_width(), image.natural_height());
        if width == 0 || height == 0 {
            return Err(JsValue::from_str("empty image"));
        }
        let scale = (f64::from(max_edge) / f64::from(width.max(height))).min(1.0);
        let (w, h) = (
            (f64::from(width) * scale).round().max(1.0),
            (f64::from(height) * scale).round().max(1.0),
        );
        let canvas: web_sys::HtmlCanvasElement = document().create_element("canvas")?.dyn_into()?;
        canvas.set_width(w as u32);
        canvas.set_height(h as u32);
        let context: web_sys::CanvasRenderingContext2d = canvas
            .get_context("2d")?
            .ok_or_else(|| JsValue::from_str("no canvas"))?
            .dyn_into()?;
        context.draw_image_with_html_image_element_and_dw_and_dh(&image, 0.0, 0.0, w, h)?;
        let blob = JsFuture::from(js_sys::Promise::new(&mut |resolve, reject| {
            let done = Closure::once_into_js(move |blob: JsValue| {
                let _ = resolve.call1(&JsValue::NULL, &blob);
            });
            if let Err(error) = canvas.to_blob_with_type_and_encoder_options(
                done.unchecked_ref(),
                "image/jpeg",
                &JsValue::from_f64(0.85),
            ) {
                let _ = reject.call1(&JsValue::NULL, &error);
            }
        }))
        .await?;
        let blob: web_sys::Blob = blob.dyn_into()?;
        let buffer = JsFuture::from(blob.array_buffer()).await?;
        Ok(js_sys::Uint8Array::new(&buffer))
    }
    .await;
    let _ = web_sys::Url::revoke_object_url(&url);
    result
}
