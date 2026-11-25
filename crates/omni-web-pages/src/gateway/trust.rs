//! "Trusted browsers": changes on this page (tokens, integrations, OAuth
//! approvals) need a browser the owner approved from their phone; viewing
//! works anywhere on the home network. See `omni_runtime::owner`.

use std::time::Duration;

use leptos::prelude::*;
use omni_api::owner::{OwnerStatus, PairingPhase, PairingStarted, TrustedBrowserView};
use omni_web_kit::api;
use omni_web_kit::components::{
    Button, ButtonSize, ButtonVariant, ConfirmButton, InlineNote, Panel, ToastKind, Tone,
};
use omni_web_kit::task::{sleep, spawn_detached, spawn_scoped};
use omni_web_kit::utils::format::format_absolute;

use super::Gateway;
use super::logic;

const CLAIM_POLL: Duration = Duration::from_secs(2);

#[component]
pub(super) fn TrustPanel(gw: Gateway) -> impl IntoView {
    let status = RwSignal::new(None::<OwnerStatus>);
    let nonce = RwSignal::new(0u64);
    let pairing = RwSignal::new(None::<PairingStarted>);
    let asking = RwSignal::new(false);

    Effect::new(move |_| {
        nonce.track();
        spawn_scoped(async move {
            match api::fetch_owner_status().await {
                Ok(next) => status.set(Some(next)),
                Err(e) => gw.toast.show(
                    format!("Could not check this browser: {}", e.message()),
                    ToastKind::Error,
                ),
            }
        });
    });

    // While a request waits, collect the cookie as soon as the phone approves.
    Effect::new(move |_| {
        let Some(started) = pairing.get() else {
            return;
        };
        spawn_scoped(async move {
            loop {
                sleep(CLAIM_POLL).await;
                let outcome = api::claim_owner_pairing(&started.id, &started.claim).await;
                let phase = match outcome {
                    Ok(state) => state.state,
                    Err(_) => continue,
                };
                match phase {
                    PairingPhase::Pending => continue,
                    PairingPhase::Trusted => gw.toast.show(
                        "This browser is trusted; you can make changes now",
                        ToastKind::Info,
                    ),
                    PairingPhase::Denied => {
                        gw.toast.show("The request was denied", ToastKind::Error);
                    }
                    PairingPhase::Expired => gw.toast.show(
                        "The request expired; ask again to trust this browser",
                        ToastKind::Error,
                    ),
                }
                pairing.try_set(None);
                nonce.try_update(|n| *n += 1);
                break;
            }
        });
    });

    let ask = move |_| {
        if asking.get_untracked() {
            return;
        }
        asking.set(true);
        spawn_detached(async move {
            match api::start_owner_pairing().await {
                Ok(started) => {
                    pairing.try_set(Some(started));
                }
                Err(e) => gw.toast.show(e.message().to_owned(), ToastKind::Error),
            }
            asking.try_set(false);
        });
    };

    let trusted = Memo::new(move |_| status.with(|s| s.as_ref().map(|s| s.trusted)));

    move || {
        match trusted.get() {
        None => None,
        Some(false) => Some(
            view! {
                <Panel title="This browser" class="gw-trust">
                    <div class="panel-body stack-sm">
                        {move || match pairing.get() {
                            None => view! {
                                <InlineNote tone=Tone::Warn role="status">
                                    "Changes on this page need a trusted browser. Viewing works anywhere on the home network."
                                </InlineNote>
                                <div class="cluster">
                                    <Button
                                        size=ButtonSize::Sm
                                        variant=ButtonVariant::Primary
                                        busy=Signal::derive(move || asking.get())
                                        on_click=Callback::new(ask)
                                    >
                                        "Trust this browser"
                                    </Button>
                                </div>
                            }
                            .into_any(),
                            Some(started) => view! {
                                <p class="small">
                                    "Your phone got a push. Open it and type this code:"
                                </p>
                                <p class="mono gw-trust-code" aria-live="polite">{started.code.clone()}</p>
                                <p class="small dim">
                                    "This page continues by itself once you approve. The request expires in ten minutes."
                                </p>
                                <div class="cluster">
                                    <Button
                                        size=ButtonSize::Sm
                                        variant=ButtonVariant::Ghost
                                        on_click=Callback::new(move |_| pairing.set(None))
                                    >
                                        "Cancel"
                                    </Button>
                                </div>
                            }
                            .into_any(),
                        }}
                    </div>
                </Panel>
            }
            .into_any(),
        ),
        Some(true) => Some(view! { <TrustedBrowsers gw status nonce/> }.into_any()),
    }
    }
}

#[component]
fn TrustedBrowsers(
    gw: Gateway,
    status: RwSignal<Option<OwnerStatus>>,
    nonce: RwSignal<u64>,
) -> impl IntoView {
    let browsers = Memo::new(move |_| {
        status.with(|s| s.as_ref().map(|s| s.browsers.clone()).unwrap_or_default())
    });
    let removing = RwSignal::new(None::<String>);
    let remove = Callback::new(move |browser: TrustedBrowserView| {
        removing.set(Some(browser.id.clone()));
        spawn_detached(async move {
            match api::revoke_trusted_browser(&browser.id).await {
                Ok(()) => gw
                    .toast
                    .show(format!("Removed {}", browser.label), ToastKind::Info),
                Err(e) => gw.toast.show(e.message().to_owned(), ToastKind::Error),
            }
            removing.try_set(None);
            nonce.try_update(|n| *n += 1);
        });
    });
    view! {
        <Panel
            title="Trusted browsers"
            class="gw-trust"
            head_end=ViewFn::from(move || view! { <span class="num">{move || browsers.with(Vec::len)}</span> })
        >
            <div class="rows">
                <For
                    each=move || browsers.get()
                    key=|b| b.id.clone()
                    children=move |browser| {
                        let created = browser.created_at as f64;
                        let seen = browser.last_seen_at.map(|at| at as f64);
                        let id = browser.id.clone();
                        let busy = Signal::derive(move || removing.with(|r| r.as_deref() == Some(id.as_str())));
                        let label = browser.label.clone();
                        let current = browser.current;
                        let target = StoredValue::new(browser);
                        view! {
                            <div class="row">
                                <span class="row-main">
                                    <span class="row-title">
                                        {label.clone()}
                                        {current.then_some(" · this browser")}
                                    </span>
                                    <span class="row-sub dot-sep">
                                        <span title=format_absolute(created)>{move || format!("trusted {}", logic::ago(created, gw.now.get()))}</span>
                                        {seen.map(|at| view! {
                                            <span title=format_absolute(at)>{move || format!("seen {}", logic::ago(at, gw.now.get()))}</span>
                                        })}
                                    </span>
                                </span>
                                <span class="row-end">
                                    <ConfirmButton
                                        label="Remove"
                                        confirm_label="Remove browser"
                                        context=format!("browser {label}")
                                        destructive=true
                                        variant=ButtonVariant::Ghost
                                        size=ButtonSize::Sm
                                        busy
                                        title="It can still view this page, but no longer make changes"
                                        on_confirm=Callback::new(move |()| remove.run(target.get_value()))
                                    />
                                </span>
                            </div>
                        }
                    }
                />
            </div>
        </Panel>
    }
}
