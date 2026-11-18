//! The screens and the pieces they share.

mod chart;
mod cook;
mod inbox;
mod library;
mod recipe;

use leptos::prelude::*;
use omni_api::fond::CookEventKind;
use wasm_bindgen::JsCast;

use crate::logic::{Route, countdown};
use crate::platform::{device, listen, window};
use crate::store::{Store, use_store};

thread_local! {
    /// In-app navigations on the history stack, so Back can tell whether
    /// there is a screen of ours to go back to.
    static DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// Goes to `route`, adding a history entry.
pub fn navigate(store: Store, route: Route) {
    if store.route.get_untracked() == Route::Library {
        store.library_scroll.set(window().scroll_y().unwrap_or(0.0));
    }
    if let Ok(history) = window().history() {
        let _ = history.push_state_with_url(&wasm_bindgen::JsValue::NULL, "", Some(&route.path()));
    }
    DEPTH.set(DEPTH.get() + 1);
    store.route.set(route);
    window().scroll_to_with_x_and_y(0.0, 0.0);
}

/// Replaces the current screen without adding a history entry.
pub fn replace(store: Store, route: Route) {
    if let Ok(history) = window().history() {
        let _ =
            history.replace_state_with_url(&wasm_bindgen::JsValue::NULL, "", Some(&route.path()));
    }
    store.route.set(route);
    window().scroll_to_with_x_and_y(0.0, 0.0);
}

/// Goes back one screen, or to `fallback` when the app was opened here.
pub fn back(store: Store, fallback: Route) {
    if DEPTH.get() > 0
        && let Ok(history) = window().history()
    {
        let _ = history.back();
    } else {
        replace(store, fallback);
    }
}

/// An in-app link: a real `<a href>` that navigates without a page load.
#[component]
pub fn Link(
    to: Route,
    #[prop(optional, into)] class: String,
    #[prop(optional, into)] label: String,
    children: Children,
) -> impl IntoView {
    let store = use_store();
    let href = to.path();
    view! {
        <a
            href=href
            class=class
            aria-label=(!label.is_empty()).then_some(label)
            on:click=move |event: web_sys::MouseEvent| {
                if event.button() == 0 && !event.meta_key() && !event.ctrl_key() {
                    event.prevent_default();
                    navigate(store, to.clone());
                }
            }
        >
            {children()}
        </a>
    }
}

pub fn icon(name: &'static str) -> impl IntoView {
    let paths = match name {
        "back" => "<path d='M15 5l-7 7 7 7'/>",
        "close" => "<path d='M6 6l12 12M18 6L6 18'/>",
        "more" => {
            "<circle cx='5' cy='12' r='1.6' fill='currentColor'/><circle cx='12' cy='12' r='1.6' fill='currentColor'/><circle cx='19' cy='12' r='1.6' fill='currentColor'/>"
        }
        "filter" => "<path d='M4 7h16M7 12h10M10 17h4'/>",
        "camera" => {
            "<path d='M4 8h3.5l1.5-2.5h6L16.5 8H20v11H4z'/><circle cx='12' cy='13.2' r='3.4'/>"
        }
        "check" => "<path d='M5 12.5l4.5 4.5L19 7'/>",
        "timer" => "<circle cx='12' cy='13.5' r='7'/><path d='M12 9.5v4l2.5 2M9.5 3.5h5'/>",
        "edit" => "<path d='M5 19l1-4L16.5 4.5l3 3L9 18z'/>",
        "next" => "<path d='M9 5l7 7-7 7'/>",
        _ => "",
    };
    view! {
        <svg class="icon" viewBox="0 0 24 24" aria-hidden="true" fill="none" stroke="currentColor"
            stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" inner_html=paths></svg>
    }
}

/// A bottom sheet. It stays in the page and is shown while `open`.
#[component]
pub fn Sheet(
    open: RwSignal<bool>,
    #[prop(into)] title: Signal<String>,
    children: Children,
) -> impl IntoView {
    view! {
        <div class="scrim" class:open=move || open.get() on:click=move |_| open.set(false)></div>
        <section class="sheet" class:open=move || open.get() role="dialog" aria-modal="true"
            aria-hidden=move || (!open.get()).to_string()>
            <header class="sheet-head">
                <h2>{move || title.get()}</h2>
                <button class="icon-btn" aria-label="Close" on:click=move |_| open.set(false)>{icon("close")}</button>
            </header>
            <div class="sheet-body">{children()}</div>
        </section>
    }
}

/// The quiet connection readout every header carries.
#[component]
pub fn Connection() -> impl IntoView {
    let store = use_store();
    let waiting = move || store.outbox.with(Vec::len);
    view! {
        <Show when={move || !store.online.get() || waiting() > 0}>
            <span class="conn" class:offline=move || !store.online.get() data-testid="connection">
                <i></i>
                {move || {
                    let waiting = waiting();
                    match (store.online.get(), waiting) {
                        (false, 0) => "Offline".to_owned(),
                        (false, n) => format!("Offline · {n} to send"),
                        (true, _) => "Sending".to_owned(),
                    }
                }}
            </span>
        </Show>
    }
}

/// Toast, timer alerts and the update notice, stacked above the screen.
#[component]
fn Overlays() -> impl IntoView {
    let store = use_store();
    let step_label = move |session: &str, step: &str| {
        let label = store.sessions.with_untracked(|sessions| {
            let session = sessions.get(session)?;
            let state = crate::logic::session::project(session);
            let (doc, _) = store.session_doc(session, &state)?;
            let ix = omni_recipe_format::Index::build(&doc);
            ix.step(step).map(|found| found.action.clone())
        });
        label.unwrap_or_else(|| step.to_owned())
    };
    view! {
        <div class="stack">
            {move || {
                store
                    .alerts
                    .get()
                    .into_iter()
                    .map(|alert| {
                        let label = step_label(&alert.session, &alert.step);
                        let (session, step) = (alert.session.clone(), alert.step.clone());
                        let open = alert.session.clone();
                        view! {
                            <div class="notice alarm" role="alert" data-testid="timer-alert">
                                <button class="notice-main" on:click=move |_| navigate(store, Route::Cook(open.clone()))>
                                    {icon("timer")}
                                    <span><b>"Timer done"</b>" · "{label}</span>
                                </button>
                                <button class="notice-act" on:click=move |_| {
                                    store.cook_event(&session, CookEventKind::CancelTimer { step: step.clone() });
                                    store.alerts.update(|alerts| alerts.retain(|a| !(a.session == session && a.step == step)));
                                }>"Stop"</button>
                            </div>
                        }
                    })
                    .collect_view()
            }}
            <Show when=move || store.update_available.get()>
                <div class="notice">
                    <span class="notice-main">"A new version of Fond is ready."</span>
                    <button class="notice-act" on:click=move |_| device::apply_update()>"Update"</button>
                </div>
            </Show>
            {move || {
                store.toast.get().map(|toast| {
                    view! {
                        <div class="notice" role="status" data-testid="toast">
                            <span class="notice-main">{toast.text}</span>
                            {toast.undo.map(|undo| view! {
                                <button class="notice-act" on:click=move |_| {
                                    undo();
                                    store.toast.set(None);
                                }>"Undo"</button>
                            })}
                        </div>
                    }
                })
            }}
        </div>
    }
}

/// A countdown to `ends_at` that follows the app clock.
pub fn remaining(store: Store, ends_at: i64) -> String {
    countdown(ends_at - store.now.get())
}

#[component]
pub fn App() -> impl IntoView {
    let store = Store::new();
    provide_context(store);
    if let Ok(history) = window().history() {
        let _ = history.set_scroll_restoration(web_sys::ScrollRestoration::Manual);
    }
    listen(&window(), "popstate", move |_| {
        DEPTH.set(DEPTH.get().saturating_sub(1));
        let path = window().location().pathname().unwrap_or_default();
        store.route.set(Route::parse(&path));
    });
    device::register_service_worker(store.update_available, move || store.cache_thumbnails());
    device::persist_storage();
    store.start();

    let route = Memo::new(move |_| store.route.get());
    // Cook mode holds the screen awake; the lock is dropped when the page is
    // hidden, so it is taken again each time the app returns to the front.
    let hold = move || {
        if matches!(store.route.get_untracked(), Route::Cook(_)) {
            leptos::task::spawn_local(async move {
                let held = device::request_wake_lock().await;
                store.wake_refused.set(!held && crate::platform::visible());
            });
        } else {
            device::release_wake_lock();
        }
    };
    Effect::new(move |_| {
        route.track();
        hold();
    });
    listen(
        &crate::platform::document(),
        "visibilitychange",
        move |_| {
            if crate::platform::visible() {
                hold();
            }
        },
    );
    // A tap anywhere unlocks sound, so a timer started on another device can ring here.
    listen(&crate::platform::document(), "pointerdown", |event| {
        if event
            .target()
            .is_some_and(|target| target.dyn_ref::<web_sys::Element>().is_some())
        {
            device::prime_audio();
        }
    });

    view! {
        <Show when=move || store.ready.get() fallback=|| view! { <div class="boot">"Fond"</div> }>
            {move || match route.get() {
                Route::Library => view! { <library::Library/> }.into_any(),
                Route::Recipe(id) => view! { <recipe::RecipePage id=id/> }.into_any(),
                Route::Cook(id) => view! { <cook::CookPage id=id/> }.into_any(),
                Route::Inbox => view! { <inbox::InboxPage/> }.into_any(),
                Route::NotFound => view! {
                    <main class="page empty">
                        <p>"There is nothing at this address."</p>
                        <Link to=Route::Library class="btn">"Go to the library"</Link>
                    </main>
                }.into_any(),
            }}
        </Show>
        <Overlays/>
    }
}
