//! `/gateway`: the MCP gateway. One downstream endpoint serves Omni's own
//! tools plus the visible tools of every enabled integration; this page sets
//! up integrations and their write-only credentials, hides tools, and manages
//! who may connect (client tokens, OAuth grants and pending approvals).
//!
//! The overview polls every 30 s, every 5 s while an OAuth request waits for
//! approval, and immediately when the tab becomes visible again. The selected
//! integration (`?integration=<id>`) opens in an inspector; the upstream OAuth
//! callback returns to `?integration=<id>&oauth=connected|error&message=…`.

mod access;
mod detail;
mod editor;
mod endpoint;
mod events;
mod integrations;
mod logic;

use std::time::Duration;

use leptos::prelude::*;
use omni_api::gateway::{
    GatewayOverview, IntegrationDetail, IntegrationStatus, IntegrationSummary,
};
use omni_web_kit::api;
use omni_web_kit::components::{
    Button, ButtonSize, ButtonVariant, ErrorState, Icon, InlineNote, PageHead, SkeletonRows,
    Status, StatusKind, ToastHandle, ToastKind, Tone, use_toast,
};
use omni_web_kit::hooks::{
    query_param, replace_query_param, scroll_into_view_center, use_now, use_page_visible,
};
use omni_web_kit::task::{sleep, spawn_detached, spawn_scoped};
use omni_web_kit::utils::js::now_ms;

use self::detail::{DetailTab, IntegrationInspector};
use self::editor::AddIntegration;

const FAST_POLL: Duration = Duration::from_secs(5);
const SLOW_POLL: Duration = Duration::from_secs(30);

/// Page state shared by the sections and inspectors.
#[derive(Clone, Copy)]
struct Gateway {
    overview: RwSignal<Option<GatewayOverview>>,
    nonce: RwSignal<u64>,
    /// The integration open in the inspector (`?integration=`).
    selected: RwSignal<Option<String>>,
    tab: RwSignal<DetailTab>,
    detail: RwSignal<Option<IntegrationDetail>>,
    detail_error: RwSignal<Option<String>>,
    /// Bumped to fetch the selected detail again.
    detail_nonce: RwSignal<u64>,
    /// Integrations with a refresh in flight, with its start time.
    refreshing: RwSignal<Vec<(String, f64)>>,
    toggling: RwSignal<Option<String>>,
    connecting: RwSignal<Option<String>>,
    editor_open: RwSignal<bool>,
    now: ReadSignal<f64>,
    toast: ToastHandle,
}

impl Gateway {
    /// Fetches the overview now (and restarts the poll timer).
    fn reload(self) {
        self.nonce.try_update(|n| *n += 1);
    }

    fn summary_untracked(self, id: &str) -> Option<IntegrationSummary> {
        self.overview
            .try_with_untracked(|o| {
                o.as_ref()?
                    .integrations
                    .iter()
                    .find(|i| i.id == id)
                    .cloned()
            })
            .flatten()
    }

    fn name_untracked(self, id: &str) -> String {
        self.summary_untracked(id)
            .map_or_else(|| id.to_owned(), |s| s.name)
    }

    fn open(self, id: String, tab: DetailTab) {
        self.editor_open.try_set(false);
        self.tab.try_set(tab);
        if self.selected.try_get_untracked().flatten().as_deref() != Some(id.as_str()) {
            self.selected.try_set(Some(id));
        }
    }

    /// Puts a mutation's result in place and reloads the overview counts.
    fn apply(self, detail: IntegrationDetail) {
        let summary = detail.summary.clone();
        self.overview.try_update(|o| {
            if let Some(o) = o {
                match o.integrations.iter_mut().find(|i| i.id == summary.id) {
                    Some(slot) => *slot = summary.clone(),
                    None => o.integrations.push(summary.clone()),
                }
            }
        });
        if self.selected.try_get_untracked().flatten().as_deref() == Some(summary.id.as_str()) {
            self.detail.try_set(Some(detail));
        }
        self.reload();
    }

    fn forget(self, id: &str) {
        self.overview.try_update(|o| {
            if let Some(o) = o {
                o.integrations.retain(|i| i.id != id);
            }
        });
        if self.selected.try_get_untracked().flatten().as_deref() == Some(id) {
            self.selected.try_set(None);
        }
        self.reload();
    }

    fn is_refreshing(self, id: &str) -> bool {
        self.refreshing.with(|r| r.iter().any(|(i, _)| i == id))
    }

    /// Re-reads the integration's catalog (up to about a minute).
    fn refresh(self, id: String) {
        if self
            .refreshing
            .try_with_untracked(|r| r.iter().any(|(i, _)| *i == id))
            .unwrap_or(true)
        {
            return;
        }
        self.refreshing
            .try_update(|r| r.push((id.clone(), now_ms())));
        let name = self.name_untracked(&id);
        spawn_detached(async move {
            let result = api::refresh_gateway_integration(&id).await;
            self.refreshing.try_update(|r| r.retain(|(i, _)| *i != id));
            match result {
                Ok(detail) => {
                    let (message, ok) = logic::refresh_outcome(&detail);
                    self.toast.show(
                        message,
                        if ok {
                            ToastKind::Info
                        } else {
                            ToastKind::Error
                        },
                    );
                    self.apply(detail);
                }
                Err(e) => {
                    self.toast.show(
                        format!("{name} did not refresh: {}", e.message()),
                        ToastKind::Error,
                    );
                    self.reload();
                }
            }
        });
    }

    fn set_enabled(self, id: String, enabled: bool) {
        if self
            .toggling
            .try_get_untracked()
            .is_none_or(|t| t.is_some())
        {
            return;
        }
        self.toggling.set(Some(id.clone()));
        let patch = omni_api::gateway::IntegrationPatch {
            enabled: Some(enabled),
            ..Default::default()
        };
        spawn_detached(async move {
            let result = api::update_gateway_integration(&id, &patch).await;
            self.toggling.try_set(None);
            match result {
                Ok(detail) => {
                    let verb = if enabled { "Enabled" } else { "Disabled" };
                    self.toast
                        .show(format!("{verb} {}", detail.summary.name), ToastKind::Info);
                    // A newly enabled integration without a catalog loads one now.
                    let load = enabled && detail.summary.status == IntegrationStatus::Pending;
                    self.apply(detail);
                    if load {
                        self.refresh(id);
                    }
                }
                Err(e) => self.toast.show(e.message().to_owned(), ToastKind::Error),
            }
        });
    }

    /// Starts the upstream OAuth sign-in and leaves for its authorize page.
    fn connect(self, id: String) {
        if self
            .connecting
            .try_get_untracked()
            .is_none_or(|c| c.is_some())
        {
            return;
        }
        self.connecting.set(Some(id.clone()));
        spawn_detached(async move {
            let failure = match api::start_gateway_oauth(&id).await {
                Ok(start) => {
                    let url = start.authorize_url;
                    let web = url.starts_with("https://") || url.starts_with("http://");
                    if web && window().location().assign(&url).is_ok() {
                        // Stay busy while the browser leaves.
                        return;
                    }
                    "the server returned an unusable sign-in address".to_owned()
                }
                Err(e) => e.message().to_owned(),
            };
            self.connecting.try_set(None);
            self.toast.show(
                format!("Could not start sign-in: {failure}"),
                ToastKind::Error,
            );
        });
    }
}

/// Copies `text` through `navigator.clipboard.writeText`; `false` when the
/// API is missing or refuses.
async fn copy_text(text: &str) -> bool {
    use wasm_bindgen::JsCast as _;
    let Ok(navigator) = js_sys::Reflect::get(&js_sys::global(), &"navigator".into()) else {
        return false;
    };
    let Ok(clipboard) = js_sys::Reflect::get(&navigator, &"clipboard".into()) else {
        return false;
    };
    let Ok(write) = js_sys::Reflect::get(&clipboard, &"writeText".into()) else {
        return false;
    };
    let Ok(write) = write.dyn_into::<js_sys::Function>() else {
        return false;
    };
    let Ok(promise) = write.call1(&clipboard, &text.into()) else {
        return false;
    };
    let Ok(promise) = promise.dyn_into::<js_sys::Promise>() else {
        return false;
    };
    wasm_bindgen_futures::JsFuture::from(promise).await.is_ok()
}

/// A copy button; "Copied" is announced beside it for two seconds. With
/// `text_label` the button shows its label instead of only the glyph.
#[component]
fn CopyButton(
    #[prop(into)] text: Signal<String>,
    #[prop(into)] label: String,
    #[prop(optional)] text_label: bool,
    #[prop(optional)] variant: Option<ButtonVariant>,
) -> impl IntoView {
    let copied = RwSignal::new(false);
    let toast = use_toast();
    let copy = move |_| {
        let value = text.get_untracked();
        spawn_detached(async move {
            if copy_text(&value).await {
                copied.try_set(true);
                sleep(Duration::from_secs(2)).await;
                copied.try_set(false);
            } else {
                toast.show(
                    "Could not copy: the browser blocked the clipboard",
                    ToastKind::Error,
                );
            }
        });
    };
    let variant = variant.unwrap_or(ButtonVariant::Ghost);
    view! {
        <span class="copy-wrap">
            {if text_label {
                view! {
                    <Button size=ButtonSize::Sm variant icon=Icon::Copy on_click=Callback::new(copy)>
                        {label.clone()}
                    </Button>
                }
                .into_any()
            } else {
                view! {
                    <Button
                        size=ButtonSize::Sm
                        variant
                        icon=Icon::Copy
                        icon_only=true
                        aria_label=label.clone()
                        title=label.clone()
                        on_click=Callback::new(copy)
                    />
                }
                .into_any()
            }}
            <span class="small text-signal copy-done" role="status">
                {move || copied.get().then_some("Copied")}
            </span>
        </span>
    }
}

/// "Refreshing · 12s": a refresh can take up to a minute, so the elapsed
/// time shows it is still going.
#[component]
fn RefreshingStatus(gw: Gateway, id: String) -> impl IntoView {
    let now = use_now(1000);
    let started = Memo::new(move |_| {
        gw.refreshing
            .with(|r| r.iter().find(|(i, _)| *i == id).map(|(_, at)| *at))
    });
    let label = Signal::derive(move || match started.get() {
        Some(at) => format!(
            "Refreshing · {}s",
            ((now.get() - at) / 1000.0).floor().max(0.0)
        ),
        None => "Refreshing".to_owned(),
    });
    view! { <Status kind=StatusKind::Running label/> }
}

/// The deferred toast for an OAuth callback (`?oauth=`).
#[derive(Clone)]
struct OAuthReturn {
    state: String,
    id: Option<String>,
    message: Option<String>,
}

#[component]
pub fn GatewayPage() -> impl IntoView {
    let toast = use_toast();
    let initial = query_param("integration").filter(|id| !id.is_empty());
    let oauth = query_param("oauth").map(|state| OAuthReturn {
        state,
        id: initial.clone(),
        message: query_param("message"),
    });
    if oauth.is_some() {
        replace_query_param("oauth", None);
        replace_query_param("message", None);
    }
    let gw = Gateway {
        overview: RwSignal::new(None),
        nonce: RwSignal::new(0),
        selected: RwSignal::new(initial),
        // A failed sign-in opens on its connection state.
        tab: RwSignal::new(match &oauth {
            Some(back) if back.state != "connected" => DetailTab::Settings,
            _ => DetailTab::Tools,
        }),
        detail: RwSignal::new(None),
        detail_error: RwSignal::new(None),
        detail_nonce: RwSignal::new(0),
        refreshing: RwSignal::new(Vec::new()),
        toggling: RwSignal::new(None),
        connecting: RwSignal::new(None),
        editor_open: RwSignal::new(false),
        now: use_now(15_000),
        toast,
    };
    let load_error = RwSignal::new(None::<String>);

    // Overview poll: 5 s while an authorization waits, else 30 s; hidden
    // tabs stop, and returning to the tab (or any mutation) fetches at once.
    let visible = use_page_visible();
    Effect::new(move |_| {
        gw.nonce.track();
        if !visible.get() {
            return;
        }
        spawn_scoped(async move {
            loop {
                match api::fetch_gateway().await {
                    Ok(next) => {
                        if gw.overview.with_untracked(|o| o.as_ref() != Some(&next)) {
                            gw.overview.set(Some(next));
                        }
                        if load_error.with_untracked(Option::is_some) {
                            load_error.set(None);
                        }
                    }
                    Err(e) => load_error.set(Some(e.message().to_owned())),
                }
                let fast = gw
                    .overview
                    .with_untracked(|o| o.as_ref().is_some_and(|o| !o.pending.is_empty()));
                sleep(if fast { FAST_POLL } else { SLOW_POLL }).await;
            }
        });
    });

    // The selected integration's detail, fetched again whenever its summary
    // changes (a refresh, an edit or a status change seen by the poll).
    let selected_summary = Memo::new(move |_| {
        let id = gw.selected.get()?;
        gw.overview.with(|o| {
            o.as_ref()?
                .integrations
                .iter()
                .find(|i| i.id == id)
                .cloned()
        })
    });
    Effect::new(move |previous: Option<Option<String>>| {
        let id = gw.selected.get();
        selected_summary.track();
        gw.detail_nonce.track();
        if previous.as_ref() != Some(&id) {
            gw.detail.set(None);
            gw.detail_error.set(None);
            replace_query_param("integration", id.as_deref());
        }
        if let Some(id) = id.clone() {
            spawn_scoped(async move {
                match api::fetch_gateway_integration(&id).await {
                    Ok(detail) => {
                        if gw.selected.get_untracked().as_deref() == Some(id.as_str()) {
                            gw.detail.set(Some(detail));
                            gw.detail_error.set(None);
                        }
                    }
                    Err(e) => gw.detail_error.set(Some(e.message().to_owned())),
                }
            });
        }
        id
    });

    let loaded = Memo::new(move |_| gw.overview.with(Option::is_some));
    let error_text = Memo::new(move |_| load_error.get());

    // The OAuth callback's toast, once names are known.
    let oauth = StoredValue::new(oauth);
    Effect::new(move |_| {
        if !loaded.get() {
            return;
        }
        let Some(back) = oauth.try_update_value(Option::take).flatten() else {
            return;
        };
        let name = back
            .id
            .as_deref()
            .map_or_else(|| "the integration".to_owned(), |id| gw.name_untracked(id));
        if back.state == "connected" {
            toast.show(format!("Connected {name}"), ToastKind::Info);
        } else {
            let detail: String = back
                .message
                .filter(|m| !m.trim().is_empty())
                .map(|m| m.chars().take(200).collect())
                .unwrap_or_else(|| "the server did not say why".to_owned());
            toast.show(
                format!("Sign-in to {name} failed: {detail}"),
                ToastKind::Error,
            );
        }
    });

    let lede = Signal::derive(move || gw.overview.with(|o| o.as_ref().map(logic::lede)));
    let pending = Memo::new(move |_| {
        gw.overview
            .with(|o| o.as_ref().map_or(0, |o| o.pending.len()))
    });
    let has_selection = Memo::new(move |_| gw.selected.with(Option::is_some));

    view! {
        <PageHead
            title="MCP gateway"
            lede
            actions=ViewFn::from(move || view! {
                // Approve is the primary while a sign-in waits.
                {move || {
                let variant = if pending.get() > 0 { ButtonVariant::Secondary } else { ButtonVariant::Primary };
                view! {
                <Button
                    variant
                    icon=Icon::Plus
                    disabled=Signal::derive(move || !loaded.get())
                    disabled_reason="The gateway is still loading"
                    on_click=Callback::new(move |_| {
                        gw.selected.set(None);
                        gw.editor_open.set(true);
                    })
                >
                    "Add integration"
                </Button>
                }
                }}
            })
        />
        {move || {
            if loaded.get() {
                return None;
            }
            Some(match error_text.get() {
                Some(e) => view! {
                    <ErrorState
                        title="The MCP gateway could not load"
                        raw=e
                        retry=Callback::new(move |()| gw.reload())
                        page=true
                    />
                }
                .into_any(),
                None => view! { <SkeletonRows count=6 label="Loading the MCP gateway"/> }.into_any(),
            })
        }}
        {move || loaded.get().then(|| view! {
            <div class="stack-lg gateway-page">
                {move || error_text.get().map(|e| view! {
                    <InlineNote tone=Tone::Warn role="status">
                        {format!("Refresh failed ({e}). Showing the last loaded state.")}
                    </InlineNote>
                })}
                {move || (pending.get() > 0).then(|| view! {
                    <div class="gw-attention" role="status">
                        <span class="status warn">
                            {move || format!(
                                "{} waiting for your approval",
                                logic::plural(pending.get(), "OAuth sign-in is", "OAuth sign-ins are")
                            )}
                        </span>
                        <Button
                            size=ButtonSize::Sm
                            on_click=Callback::new(|_| scroll_into_view_center("gateway-pending"))
                        >
                            "Review"
                        </Button>
                    </div>
                })}
                <endpoint::EndpointOverview gw/>
                <div class="split gw-split">
                    <integrations::IntegrationsPanel gw/>
                    <access::AccessColumn gw/>
                </div>
                <events::FeedsPanel gw/>
            </div>
        })}
        {move || gw.editor_open.get().then(|| view! {
            <AddIntegration gw on_close=Callback::new(move |()| gw.editor_open.set(false))/>
        })}
        {move || (has_selection.get() && !gw.editor_open.get()).then(|| view! {
            <IntegrationInspector gw on_close=Callback::new(move |()| gw.selected.set(None))/>
        })}
    }
}
