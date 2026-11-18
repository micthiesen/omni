//! The inbox: photos of a cookbook page or recipe card, sent to Omni for an
//! agent to turn into a recipe.

use leptos::html;
use leptos::prelude::*;
use leptos::task::spawn_local;

use super::{Connection, Link, back, icon};
use crate::logic::Route;
use crate::logic::outbox::{inbox_items, inbox_remaining};
use crate::store::use_store;

#[component]
pub fn InboxPage() -> impl IntoView {
    let store = use_store();
    let camera = NodeRef::<html::Input>::new();
    let library = NodeRef::<html::Input>::new();
    spawn_local(async move { store.load_inbox().await });

    let picked = move |input: NodeRef<html::Input>| {
        let Some(input) = input.get_untracked() else {
            return;
        };
        let files: Vec<web_sys::File> = input
            .files()
            .map(|list| (0..list.length()).filter_map(|n| list.get(n)).collect())
            .unwrap_or_default();
        // Clearing lets the same photo be chosen again.
        input.set_value("");
        if !files.is_empty() {
            spawn_local(async move { store.add_inbox_photos(files).await });
        }
    };
    let uploading = move || store.outbox.with(|entries| inbox_items(entries));

    view! {
        <main class="page inbox">
            <header class="bar">
                <button class="icon-btn" aria-label="Back" on:click=move |_| back(store, Route::Library)>{icon("back")}</button>
                <h1>"Inbox"</h1>
                <Connection/>
            </header>
            <div class="body">
                <p class="lede">
                    "Photograph a cookbook page or a recipe card. Several photos sent together become one recipe; an agent does the reading."
                </p>
                <div class="capture">
                    <label class="btn primary big">
                        {icon("camera")}"Take photos"
                        <input node_ref=camera type="file" accept="image/*" capture="environment" multiple class="visually-hidden"
                            data-testid="camera-input" on:change=move |_| picked(camera)/>
                    </label>
                    <label class="btn big">
                        "Choose from library"
                        <input node_ref=library type="file" accept="image/*" multiple class="visually-hidden"
                            data-testid="library-input" on:change=move |_| picked(library)/>
                    </label>
                </div>
                {move || store.preparing.get().map(|(done, total)| view! {
                    <p class="item" data-testid="preparing">
                        <span>{format!("Preparing photo {} of {total}…", done + 1)}</span>
                    </p>
                })}
                <ul class="items" data-testid="inbox-items">
                    {move || {
                        uploading()
                            .into_iter()
                            .map(|(item, count, _)| {
                                let left = store.outbox.with(|entries| inbox_remaining(entries, &item));
                                let sent = count.saturating_sub(left);
                                view! {
                                    <li class="item" data-state="uploading">
                                        <div>
                                            <b>{format!("{count} photo{}", if count == 1 { "" } else { "s" })}</b>
                                            <span class="muted">
                                                {if store.online.get() {
                                                    format!("Sending {} of {count}", (sent + 1).min(count))
                                                } else {
                                                    "Waiting for a connection".to_owned()
                                                }}
                                            </span>
                                        </div>
                                        <progress max=count value=sent></progress>
                                    </li>
                                }
                            })
                            .collect_view()
                    }}
                    {move || {
                        let sending: Vec<String> = uploading().into_iter().map(|(item, _, _)| item).collect();
                        store
                            .inbox
                            .get()
                            .into_iter()
                            .filter(|item| !sending.contains(&item.id))
                            .map(|item| {
                                let title = item.title.clone().unwrap_or_else(|| match (item.kind.as_str(), item.files) {
                                    ("photo", 1) => "1 photo".to_owned(),
                                    ("photo", n) => format!("{n} photos"),
                                    ("url", _) => "Web page".to_owned(),
                                    _ => "Text".to_owned(),
                                });
                                let state = match item.state.as_str() {
                                    "pending" => "Waiting for an agent",
                                    "imported" => "Imported",
                                    "duplicate" => "Already in the library",
                                    "discarded" => "Discarded",
                                    _ => "",
                                };
                                let recipe = item
                                    .recipe_id
                                    .clone()
                                    .filter(|id| store.recipes.with(|recipes| recipes.contains_key(id)));
                                view! {
                                    <li class="item" data-state=item.state.clone() data-item=item.id.clone()>
                                        <div>
                                            <b>{title}</b>
                                            <span class="muted">{state}</span>
                                        </div>
                                        {recipe.map(|id| view! { <Link to=Route::Recipe(id) class="link">"Open recipe"</Link> })}
                                    </li>
                                }
                            })
                            .collect_view()
                    }}
                </ul>
            </div>
        </main>
    }
}
