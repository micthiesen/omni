//! Choices remembered on this device, in localStorage. Storage that is
//! blocked or throws (private browsing, a full quota) reads as nothing stored
//! and drops writes, so the app keeps its defaults.

use super::window;

/// How a recipe is read. Cook mode's Steps and Chart tabs are the same
/// choice, so both screens share it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecipeView {
    Steps,
    Chart,
}

const VIEW_KEY: &str = "fond.view";

fn storage() -> Option<web_sys::Storage> {
    window().local_storage().ok().flatten()
}

/// The view chosen last on this device.
pub fn recipe_view() -> Option<RecipeView> {
    match storage()?.get_item(VIEW_KEY).ok()??.as_str() {
        "steps" => Some(RecipeView::Steps),
        "chart" => Some(RecipeView::Chart),
        _ => None,
    }
}

pub fn set_recipe_view(view: RecipeView) {
    let value = match view {
        RecipeView::Steps => "steps",
        RecipeView::Chart => "chart",
    };
    if let Some(storage) = storage() {
        let _ = storage.set_item(VIEW_KEY, value);
    }
}
