//! Fond, the recipe phone app: a Leptos CSR progressive web app served on its
//! own host (docs/recipes-app.md). `logic` is pure and tested natively;
//! everything else needs a browser.

pub mod logic;
pub mod platform;
pub mod store;
pub mod ui;

/// Mounts the app on `#root`.
pub fn run() {
    console_error_panic_hook::set_once();
    let Some(root) = platform::document().get_element_by_id("root") else {
        return;
    };
    use wasm_bindgen::JsCast;
    let Ok(root) = root.dyn_into::<web_sys::HtmlElement>() else {
        return;
    };
    leptos::mount::mount_to(root, ui::App).forget();
}
