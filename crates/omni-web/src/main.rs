//! Omni single-page app (Leptos CSR, built by trunk).
//!
//! The app only mounts on `wasm32`; native builds exist for unit tests, where
//! the view code is unreachable.
#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

mod app;
mod pages;
mod routes;
mod shell;

fn main() {
    console_error_panic_hook::set_once();
    #[cfg(target_arch = "wasm32")]
    {
        use leptos::prelude::document;
        use wasm_bindgen::JsCast as _;
        let root = document()
            .get_element_by_id("root")
            .and_then(|el| el.dyn_into::<web_sys::HtmlElement>().ok());
        match root {
            Some(root) => leptos::mount::mount_to(root, app::App).forget(),
            None => leptos::mount::mount_to_body(app::App),
        }
    }
}
