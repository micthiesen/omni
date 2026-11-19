//! Requests for the kitchen agent: the sheet that writes one ("Ask Kitchen")
//! and the page that lists them. Fond holds no conversation: the page shows
//! what was asked and where it stands, and nothing else.

use leptos::html;
use leptos::prelude::*;
use omni_api::fond::{KitchenRequest, RequestStatus};
use wasm_bindgen::JsValue;

use super::{Connection, Link, Sheet, back, icon, navigate, settled};
use crate::logic::Route;
use crate::logic::outbox::pending_requests;
use crate::store::{Ask, use_store};

/// Characters a request holds; the server refuses more.
const MAX_TEXT: &str = "2000";

/// The two entries every overflow menu has: write a request, see them all.
/// `menu` is the sheet they sit in and `ask` the sheet the first one opens.
pub fn kitchen_items(menu: RwSignal<bool>, ask: RwSignal<bool>) -> impl IntoView {
    let store = use_store();
    view! {
        <button data-testid="ask-kitchen" on:click=move |_| {
            menu.set(false);
            ask.set(true);
        }>"Ask Kitchen…"</button>
        <button data-testid="open-requests" on:click=move |_| {
            menu.set(false);
            navigate(store, Route::Requests);
        }>"Requests"</button>
    }
}

/// The sheet that writes a request: a text field and Submit. `about` is read
/// when the sheet opens and again when it sends, so it describes what is on
/// screen then.
#[component]
pub fn AskSheet(open: RwSignal<bool>, about: Callback<(), Ask>) -> impl IntoView {
    let store = use_store();
    let area = NodeRef::<html::Textarea>::new();
    // What was typed stays when the sheet is closed without sending, and
    // when the screen under it is redrawn.
    let text = store.draft;
    let line = Memo::new(move |_| {
        if open.get() {
            about.run(()).about
        } else {
            String::new()
        }
    });
    Effect::new(move |_| {
        if open.get()
            && let Some(area) = area.get()
        {
            let draft = text.get_untracked();
            if area.value() != draft {
                area.set_value(&draft);
            }
            let _ = area.focus();
        }
    });
    let submit = move |_| {
        let written = text.get_untracked();
        if written.trim().is_empty() {
            return;
        }
        store.ask(&written, about.run(()));
        text.set(String::new());
        if let Some(area) = area.get_untracked() {
            area.set_value("");
        }
        open.set(false);
    };
    view! {
        <Sheet open=open title="Ask Kitchen">
            <p class="muted" data-testid="ask-about">{move || line.get()}</p>
            <textarea
                node_ref=area
                class="ask-text"
                rows="4"
                maxlength=MAX_TEXT
                placeholder="What should change?"
                aria-label="Request"
                data-testid="ask-text"
                on:input=move |event| text.set(event_target_value(&event))
            ></textarea>
            <div class="sheet-actions">
                <button class="btn" on:click=move |_| open.set(false)>"Cancel"</button>
                <button class="btn primary" data-testid="ask-submit"
                    disabled=move || text.with(|text| text.trim().is_empty())
                    on:click=submit>"Submit"</button>
            </div>
        </Sheet>
    }
}

/// "Nov 18" in the phone's language.
fn day(at: i64) -> String {
    let options = js_sys::Object::new();
    let _ = js_sys::Reflect::set(&options, &"month".into(), &"short".into());
    let _ = js_sys::Reflect::set(&options, &"day".into(), &"numeric".into());
    #[allow(clippy::cast_precision_loss)]
    let date = js_sys::Date::new(&JsValue::from_f64(at as f64));
    date.to_locale_date_string("default", &options)
        .as_string()
        .unwrap_or_default()
}

fn status_label(status: RequestStatus) -> &'static str {
    match status {
        RequestStatus::Queued => "Queued",
        RequestStatus::Working => "Working",
        RequestStatus::Done => "Done",
        RequestStatus::Declined => "Declined",
    }
}

/// The requests to list: `(request, sent)`, the unsent ones first.
type Listed = Vec<(KitchenRequest, bool)>;

#[component]
pub fn RequestsPage() -> impl IntoView {
    let store = use_store();
    let ask = RwSignal::new(false);
    leptos::task::spawn_local(async move { store.load_requests().await });

    let latest = Memo::new(move |_| -> Listed {
        let unsent = store.outbox.with(|entries| pending_requests(entries));
        let mut listed: Listed = Vec::new();
        store.requests.with(|sent| {
            for request in sent {
                if !unsent.iter().any(|waiting| waiting.id == request.id) {
                    listed.push((request.clone(), true));
                }
            }
        });
        let mut all: Listed = unsent.into_iter().map(|request| (request, false)).collect();
        all.append(&mut listed);
        all
    });
    // Nothing moves under the sheet while a request is being written.
    let shown = settled(
        store,
        "Requests updated",
        latest.into(),
        |a: &Listed, b: &Listed| a == b,
        ask.into(),
    );

    view! {
        <main class="page requests">
            <header class="bar">
                <button class="icon-btn" aria-label="Back" on:click=move |_| back(store, Route::Library)>{icon("back")}</button>
                <h1>"Requests"</h1>
                <Connection/>
            </header>
            <div class="scroller" data-testid="scroller"><div class="scroller-in">
            <div class="body">
                <p class="lede">
                    "What you asked Kitchen to change. Kitchen answers in your chat with it; this page only shows where each request stands."
                </p>
                <button class="btn primary big" data-testid="ask-kitchen" on:click=move |_| ask.set(true)>
                    "Ask Kitchen…"
                </button>
                <ul class="requests-list" data-testid="requests">
                    {move || {
                        shown
                            .get()
                            .into_iter()
                            .map(|(request, sent)| {
                                let status = if sent { status_label(request.status) } else { "Waiting to send" };
                                let in_library = |id: &String| store.recipes.with_untracked(|recipes| recipes.contains_key(id));
                                let title = request.recipe_title.clone().or_else(|| {
                                    request
                                        .recipe_id
                                        .as_ref()
                                        .and_then(|id| store.recipes.with_untracked(|recipes| recipes.get(id).map(|recipe| recipe.title.clone())))
                                });
                                let recipe = request.recipe_id.clone().zip(title).map(|(id, title)| {
                                    if in_library(&id) {
                                        view! { <Link to=Route::Recipe(id) class="link">{title}</Link> }.into_any()
                                    } else {
                                        view! { <span class="muted">{title}</span> }.into_any()
                                    }
                                });
                                let result = (request.status == RequestStatus::Done)
                                    .then_some(request.result_revision)
                                    .flatten()
                                    .map(|revision| {
                                        let label = format!("See revision {revision}");
                                        match request.result_recipe_id.clone().or_else(|| request.recipe_id.clone()).filter(in_library) {
                                            Some(id) => view! {
                                                <Link to=Route::Recipe(id) class="link">
                                                    <span data-testid="request-result">{label}</span>
                                                </Link>
                                            }
                                            .into_any(),
                                            None => view! { <span class="muted" data-testid="request-result">{format!("Revision {revision}")}</span> }.into_any(),
                                        }
                                    });
                                let state = if sent { request.status.as_str() } else { "unsent" };
                                view! {
                                    <li class="request" data-request=request.id.clone() data-status=state>
                                        <p class="request-text">{request.text.clone()}</p>
                                        <p class="request-meta">
                                            <span class="status" data-status=state data-testid="request-status">{status}</span>
                                            <span class="muted">{day(request.at)}</span>
                                        </p>
                                        {request.note.clone().map(|note| view! { <p class="request-note" data-testid="request-note">{note}</p> })}
                                        {(recipe.is_some() || result.is_some()).then(|| view! {
                                            <p class="request-links">{recipe}{result}</p>
                                        })}
                                    </li>
                                }
                            })
                            .collect_view()
                    }}
                </ul>
                <Show when=move || shown.with(Vec::is_empty)>
                    <p class="empty" data-testid="requests-empty">"No requests yet."</p>
                </Show>
            </div>
            </div></div>
        </main>
        <AskSheet open=ask about=Callback::new(|()| Ask::library())/>
    }
}
