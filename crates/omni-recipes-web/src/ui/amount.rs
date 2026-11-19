//! The amount sheet: the one edit of a recipe the app makes. It changes the
//! quantity and unit of one ingredient, in the recipe itself.

use leptos::prelude::*;
use omni_recipe_format::{
    Index, amount_ops, apply_ops, ingredient_line, parse_qty, qty_text, unit_choices, validate,
};

use super::Sheet;
use crate::store::{Store, use_store};

/// The ingredient an amount sheet is editing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AmountTarget {
    pub recipe: String,
    pub ingredient: String,
    /// The cook session the sheet was opened from. It moves to the changed
    /// recipe, and its scale is the one on screen.
    pub session: Option<String>,
}

/// What the sheet would save, or why it cannot.
#[derive(Debug, Clone, PartialEq)]
enum Preview {
    /// The ingredient line as it will read, and the amount behind it.
    Line(String, f64, Option<String>),
    Unchanged(String),
    Refused(String),
}

fn preview(store: Store, target: &AmountTarget, qty: &str, unit: &str) -> Preview {
    let Some(doc) = store
        .recipe(&target.recipe)
        .and_then(|recipe| recipe.doc.clone())
    else {
        return Preview::Refused("This recipe is not on this phone.".to_owned());
    };
    let Some(qty) = parse_qty(qty) else {
        return Preview::Refused("Enter an amount such as 2, 1.5 or 1 1/2.".to_owned());
    };
    let unit = (!unit.is_empty()).then(|| unit.to_owned());
    let line = |doc: &omni_recipe_format::RecipeDoc| {
        let ix = Index::build(doc);
        ix.ingredient(&target.ingredient)
            .map(|ingredient| ingredient_line(&ix, ingredient, 1.0))
            .unwrap_or_default()
    };
    match amount_ops(&doc, &target.ingredient, qty, unit.as_deref()) {
        Err(refused) => Preview::Refused(sentence(&refused.to_string())),
        Ok(ops) if ops.is_empty() => Preview::Unchanged(line(&doc)),
        Ok(ops) => match apply_ops(&doc, &ops) {
            Err(error) => Preview::Refused(sentence(&error.to_string())),
            Ok(changed) => match validate(&changed).errors.first() {
                Some(problem) => Preview::Refused(sentence(&problem.message)),
                None => Preview::Line(line(&changed), qty, unit),
            },
        },
    }
}

fn sentence(text: &str) -> String {
    let mut chars = text.chars();
    let first = chars.next().map(|ch| ch.to_uppercase().collect::<String>());
    format!(
        "{}{}.",
        first.unwrap_or_default(),
        chars.as_str().trim_end_matches('.')
    )
}

/// Saves an amount and offers to put the old one back.
fn save(store: Store, target: &AmountTarget, qty: f64, unit: Option<String>) {
    let Some((old_qty, old_unit)) = store.change_amount(
        &target.recipe,
        &target.ingredient,
        qty,
        unit,
        target.session.as_deref(),
    ) else {
        return;
    };
    let target = target.clone();
    store.show_toast(
        "Amount changed in the recipe",
        Some(std::sync::Arc::new(move || {
            store.change_amount(
                &target.recipe,
                &target.ingredient,
                old_qty,
                old_unit.clone(),
                target.session.as_deref(),
            );
        })),
    );
}

const QUICK: [(&str, f64); 4] = [("½×", 0.5), ("¾×", 0.75), ("1¼×", 1.25), ("2×", 2.0)];

#[component]
pub fn AmountSheet(open: RwSignal<bool>, target: RwSignal<Option<AmountTarget>>) -> impl IntoView {
    let store = use_store();
    let qty = RwSignal::new(String::new());
    let unit = RwSignal::new(String::new());
    let units = RwSignal::new(Vec::<String>::new());
    let name = RwSignal::new(String::new());
    // Opening the sheet fills it with the amount the recipe has now.
    Effect::new(move |_| {
        if !open.get() {
            return;
        }
        // Untracked: a sync that lands while the sheet is open must not
        // replace what is being typed.
        let found = target.get().and_then(|target| {
            let recipe = store
                .recipes
                .with_untracked(|recipes| recipes.get(&target.recipe).cloned())?;
            let doc = recipe.doc.clone()?;
            let ingredient = Index::build(&doc).ingredient(&target.ingredient)?.clone();
            Some(ingredient)
        });
        if let Some(ingredient) = found {
            qty.set(ingredient.qty.map(qty_text).unwrap_or_default());
            units.set(unit_choices(ingredient.unit.as_deref()));
            unit.set(ingredient.unit.clone().unwrap_or_default());
            name.set(ingredient.name);
        }
    });
    let shown = Memo::new(move |_| {
        target
            .get()
            .filter(|_| open.get())
            .map(|target| preview(store, &target, &qty.get(), &unit.get()))
    });
    let scaled = move || {
        let target = target.get()?;
        let factor = target
            .session
            .as_ref()
            .and_then(|id| {
                store
                    .sessions
                    .with(|sessions| sessions.get(id).map(|session| session.scale))
            })
            .unwrap_or_else(|| store.scale(&target.recipe));
        #[allow(clippy::float_cmp)]
        if factor == 1.0 {
            return None;
        }
        let doc = store.recipe(&target.recipe)?.doc.clone()?;
        Some(format!(
            "The recipe is shown scaled. This is the amount for the recipe as written ({} {}).",
            qty_text(doc.r#yield.qty),
            doc.r#yield.unit
        ))
    };
    let confirm = move |_| {
        if let (Some(target), Some(Preview::Line(_, qty, unit))) =
            (target.get_untracked(), shown.get_untracked())
        {
            save(store, &target, qty, unit);
        }
        open.set(false);
    };
    view! {
        <Sheet open=open title=Signal::derive(move || format!("Change amount: {}", name.get()))>
            <p class="muted" data-testid="amount-note">
                "This changes the recipe from now on. Only the amount can be changed here."
            </p>
            {move || scaled().map(|note| view! { <p class="amount-scaled" data-testid="amount-scaled">{note}</p> })}
            <div class="amount-fields">
                <label class="field">
                    <span>"Amount"</span>
                    <input type="text" inputmode="decimal" autocomplete="off" enterkeyhint="done"
                        data-testid="amount-qty"
                        prop:value=move || qty.get()
                        on:input=move |event| qty.set(event_target_value(&event))/>
                </label>
                <label class="field">
                    <span>"Unit"</span>
                    <select data-testid="amount-unit"
                        prop:value=move || unit.get()
                        on:change=move |event| unit.set(event_target_value(&event))>
                        <option value="" selected=move || unit.get().is_empty()>"(count)"</option>
                        {move || units
                            .get()
                            .into_iter()
                            .map(|choice| {
                                let value = choice.clone();
                                view! {
                                    <option value=choice.clone() selected=move || unit.get() == value>{choice.clone()}</option>
                                }
                            })
                            .collect_view()}
                    </select>
                </label>
            </div>
            <div class="amount-quick" role="group" aria-label="Multiply the amount">
                {QUICK
                    .into_iter()
                    .map(|(label, by)| view! {
                        <button class="chip" on:click=move |_| {
                            if let Some(value) = parse_qty(&qty.get_untracked()) {
                                qty.set(qty_text(value * by));
                            }
                        }>{label}</button>
                    })
                    .collect_view()}
            </div>
            {move || match shown.get() {
                Some(Preview::Line(line, ..)) => view! {
                    <p class="amount-preview" data-testid="amount-preview">{line}</p>
                }.into_any(),
                Some(Preview::Unchanged(line)) => view! {
                    <p class="amount-preview muted" data-testid="amount-preview">{line}</p>
                }.into_any(),
                Some(Preview::Refused(why)) => view! {
                    <p class="amount-refused" data-testid="amount-refused">{why}</p>
                }.into_any(),
                None => ().into_any(),
            }}
            <div class="sheet-actions">
                <button class="btn" on:click=move |_| open.set(false)>"Cancel"</button>
                <button class="btn primary" data-testid="amount-save"
                    disabled=move || !matches!(shown.get(), Some(Preview::Line(..)))
                    on:click=confirm>"Save"</button>
            </div>
        </Sheet>
    }
}
