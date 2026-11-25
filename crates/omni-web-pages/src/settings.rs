//! `/settings`: every configuration value Omni reads, where its value comes
//! from (stored in Omni, the container environment or the built-in default),
//! and a form to change it without SSH. Saved values apply after a restart,
//! which the page can ask for; it then waits for Omni to answer again.
//! Loads once and again after every change. Values show only in a trusted
//! browser, and secret values never.

use std::collections::BTreeMap;
use std::time::Duration;

use leptos::html::{Input, Textarea};
use leptos::prelude::*;
use omni_api::common::Ms;
use omni_api::settings::{SettingSource, SettingView, SettingsView};
use omni_web_kit::api::{self, ApiClientError};
use omni_web_kit::components::{
    Button, ButtonSize, ButtonVariant, ConfirmButton, EmptyState, ErrorState, Icon, InlineNote,
    PageHead, Panel, SearchField, SkeletonRows, Status, StatusKind, Tag, ToastHandle, ToastKind,
    Tone, use_toast,
};
use omni_web_kit::hooks::use_now;
use omni_web_kit::router::Link;
use omni_web_kit::task::{sleep, spawn_detached, spawn_scoped};
use omni_web_kit::utils::format::{format_absolute, format_relative_at};
use omni_web_kit::utils::js::now_ms;

const LEDE: &str =
    "Saved values apply after a restart. Stored values win over the container environment.";
const READ_ONLY_TITLE: &str = "Omni needs it before it reads its settings, or the image sets it, \
                               so only the container environment can change it.";
const RESTART_POLL: Duration = Duration::from_secs(1);
/// A proxy in front of a stopping Omni can hold a request open.
const HEALTH_TIMEOUT: Duration = Duration::from_secs(4);
/// Shutdown waits up to 30 s for running work before the container starts
/// again, so three minutes without an answer means something is wrong.
const RESTART_LIMIT: Duration = Duration::from_secs(180);

fn source_label(source: SettingSource) -> &'static str {
    match source {
        SettingSource::Stored => "Stored",
        SettingSource::Environment => "Environment",
        SettingSource::Default => "Default",
    }
}

fn source_title(source: SettingSource) -> &'static str {
    match source {
        SettingSource::Stored => "Saved in Omni; it wins over the container environment",
        SettingSource::Environment => "From the container environment until a value is saved here",
        SettingSource::Default => "Not set anywhere: the built-in default applies",
    }
}

/// Every whitespace-separated term of `query` appears in the key or the
/// group, ignoring case ("plex url" finds `PLEX_URL`).
fn matches(setting: &SettingView, query: &str) -> bool {
    let key = setting.key.to_lowercase();
    let group = setting.group.to_lowercase();
    query
        .split_whitespace()
        .map(str::to_lowercase)
        .all(|term| key.contains(&term) || group.contains(&term))
}

/// The keys to list by group: groups sorted by name, keys sorted within.
/// `keep` (the key whose form is open) stays listed whatever the filter, so
/// typing in the filter never hides an open form.
fn grouped(
    settings: &[SettingView],
    query: &str,
    keep: Option<&str>,
) -> Vec<(String, Vec<String>)> {
    let mut groups: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    for setting in settings {
        if keep == Some(setting.key.as_str()) || matches(setting, query) {
            groups
                .entry(setting.group.as_str())
                .or_default()
                .push(setting.key.clone());
        }
    }
    groups
        .into_iter()
        .map(|(group, mut keys)| {
            keys.sort();
            (group.to_owned(), keys)
        })
        .collect()
}

fn count_text(shown: usize, total: usize, filtering: bool) -> String {
    if filtering {
        format!("{shown} of {total}")
    } else if total == 1 {
        "1 setting".to_owned()
    } else {
        format!("{total} settings")
    }
}

/// What a row says about its value.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Shown {
    Value(String),
    Empty,
    /// Set, but not shown: a secret, or a browser that is not trusted.
    Set,
    NotSet,
}

fn shown(setting: &SettingView) -> Shown {
    match &setting.value {
        Some(value) if value.is_empty() => Shown::Empty,
        Some(value) => Shown::Value(value.clone()),
        None if setting.set => Shown::Set,
        None => Shown::NotSet,
    }
}

/// Why a row's Edit button is disabled, if it is.
fn edit_blocker(values_hidden: bool, restarting: bool, other_open: bool) -> Option<&'static str> {
    if values_hidden {
        Some("Trust this browser on the MCP gateway page to change settings")
    } else if restarting {
        Some("Omni is restarting")
    } else if other_open {
        Some("Finish the open setting first")
    } else {
        None
    }
}

/// Why `value` is not sent. Omni treats an empty value as unset, so saving
/// one would only hide the environment value.
fn draft_problem(value: &str, stored: bool) -> Option<&'static str> {
    if !value.trim().is_empty() {
        None
    } else if stored {
        Some("Type a value. To remove the stored one, use the environment value instead.")
    } else {
        Some("Type a value.")
    }
}

fn value_help(setting: &SettingView) -> &'static str {
    match (setting.secret, setting.set) {
        (true, true) => {
            "Write-only: type a new value to replace the current one. Omni never shows it."
        }
        (true, false) => "Write-only: Omni never shows it once saved.",
        (false, _) => "Saving checks the whole configuration first.",
    }
}

fn saved_message(setting: &SettingView) -> String {
    if setting.restart_needed {
        format!("Saved {}. It applies after a restart.", setting.key)
    } else {
        format!("Saved {}. Omni already uses this value.", setting.key)
    }
}

fn cleared_message(setting: &SettingView) -> String {
    let source = match setting.source {
        SettingSource::Environment => "the environment value",
        SettingSource::Default => "the built-in default",
        SettingSource::Stored => "a stored value",
    };
    let when = if setting.restart_needed {
        " after a restart"
    } else {
        ""
    };
    format!("{} uses {source}{when}.", setting.key)
}

/// The "remove the stored value" button of a stored row, named after what
/// the setting falls back to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ClearAction {
    label: &'static str,
    confirm: &'static str,
    title: &'static str,
    /// The stored copy is the only one: a secret Omni cannot show and the
    /// environment does not set.
    loses_value: bool,
}

fn clear_action(setting: &SettingView) -> ClearAction {
    match (setting.environment_set, setting.secret) {
        (true, _) => ClearAction {
            label: "Use environment value",
            confirm: "Remove stored value",
            title: "Removes the stored value; the container environment's value applies after \
                    a restart.",
            loses_value: false,
        },
        (false, false) => ClearAction {
            label: "Use default",
            confirm: "Remove stored value",
            title: "Removes the stored value; the environment does not set it, so the built-in \
                    default applies after a restart.",
            loses_value: false,
        },
        (false, true) => ClearAction {
            label: "Use default",
            confirm: "Lose the stored copy",
            title: "Removes the only copy of this secret: the environment does not set it and \
                    Omni never shows it. The built-in default applies after a restart.",
            loses_value: true,
        },
    }
}

/// Whether `view` comes from a process started after the restart was asked
/// for (`before` is the boot time the page saw then).
fn restarted(before: Ms, view: &SettingsView) -> bool {
    view.booted_at != before
}

/// The page clock ticks every 30 s, so a save newer than the last tick
/// reads "just now", never "in 7s".
fn ago(at: f64, now: f64) -> String {
    format_relative_at(at.min(now - 1.0), now)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Restart {
    Idle,
    /// The restart request is in flight.
    Asking,
    /// Omni accepted it; waiting for a process booted after this time.
    Waiting(Ms),
}

#[derive(Clone, Copy)]
struct Page {
    data: RwSignal<Option<SettingsView>>,
    nonce: RwSignal<u64>,
    /// The key whose form is open; one at a time.
    editing: RwSignal<Option<String>>,
    restart: RwSignal<Restart>,
    now: ReadSignal<f64>,
    toast: ToastHandle,
}

impl Page {
    fn reload(self) {
        self.nonce.try_update(|n| *n += 1);
    }

    fn close(self, key: &str) {
        self.editing.try_update(|open| {
            if open.as_deref() == Some(key) {
                *open = None;
            }
        });
    }

    /// Puts a change's result in place, then loads every setting again.
    fn apply(self, setting: SettingView) {
        self.data.try_update(|data| {
            if let Some(data) = data {
                if let Some(slot) = data.settings.iter_mut().find(|s| s.key == setting.key) {
                    *slot = setting;
                }
                data.restart_needed = data.settings.iter().any(|s| s.restart_needed);
            }
        });
        self.reload();
    }

    fn refused(self, what: String, error: &ApiClientError) {
        self.toast
            .show(format!("{what}: {}", error.message()), ToastKind::Error);
        // Trust was withdrawn: the reload hides the values and the forms.
        if error.status() == Some(403) {
            self.reload();
        }
    }
}

/// Polls until a process booted after `before` answers, then returns its
/// settings; `None` after [`RESTART_LIMIT`]. Health is the cheap liveness
/// probe; the settings' `bootedAt` decides.
async fn wait_for_restart(before: Ms) -> Option<SettingsView> {
    let started = now_ms();
    let limit = RESTART_LIMIT.as_millis() as f64;
    while now_ms() - started < limit {
        sleep(RESTART_POLL).await;
        if api::check_health(HEALTH_TIMEOUT).await.is_err() {
            continue;
        }
        if let Ok(view) = api::fetch_settings().await
            && restarted(before, &view)
        {
            return Some(view);
        }
    }
    None
}

#[component]
pub fn SettingsPage() -> impl IntoView {
    let page = Page {
        data: RwSignal::new(None),
        nonce: RwSignal::new(0),
        editing: RwSignal::new(None),
        restart: RwSignal::new(Restart::Idle),
        now: use_now(30_000),
        toast: use_toast(),
    };
    let load_error = RwSignal::new(None::<String>);
    let filter = RwSignal::new(String::new());

    Effect::new(move |_| {
        page.nonce.track();
        spawn_scoped(async move {
            match api::fetch_settings().await {
                Ok(next) => {
                    if page.data.with_untracked(|d| d.as_ref() != Some(&next)) {
                        page.data.set(Some(next));
                    }
                    if load_error.with_untracked(Option::is_some) {
                        load_error.set(None);
                    }
                }
                Err(e) => load_error.set(Some(e.message().to_owned())),
            }
        });
    });

    Effect::new(move |_| {
        let Restart::Waiting(before) = page.restart.get() else {
            return;
        };
        spawn_scoped(async move {
            match wait_for_restart(before).await {
                Some(view) => {
                    let pending = view.restart_needed;
                    page.data.set(Some(view));
                    load_error.set(None);
                    if pending {
                        page.toast.show(
                            "Omni restarted, but saved values are still not in effect. Omni \
                             ignores stored values that do not validate at boot; its log says why.",
                            ToastKind::Error,
                        );
                    } else {
                        page.toast
                            .show("Omni restarted with the saved values", ToastKind::Info);
                    }
                }
                None => {
                    page.toast.show(
                        "Omni has not answered for 3 minutes since the restart. Reload the page \
                         to check again.",
                        ToastKind::Error,
                    );
                    page.reload();
                }
            }
            page.restart.set(Restart::Idle);
        });
    });

    let ask_restart = Callback::new(move |()| {
        if page.restart.get_untracked() != Restart::Idle {
            return;
        }
        let Some(before) = page
            .data
            .with_untracked(|d| d.as_ref().map(|d| d.booted_at))
        else {
            return;
        };
        page.restart.set(Restart::Asking);
        spawn_detached(async move {
            match api::restart_omni().await {
                Ok(()) => {
                    page.restart.try_set(Restart::Waiting(before));
                }
                Err(e) => {
                    page.refused("Omni did not restart".to_owned(), &e);
                    page.restart.try_set(Restart::Idle);
                }
            }
        });
    });

    let loaded = Memo::new(move |_| page.data.with(Option::is_some));
    let hidden = Memo::new(move |_| {
        page.data
            .with(|d| d.as_ref().is_some_and(|d| d.values_hidden))
    });
    let needs_restart = Memo::new(move |_| {
        page.data
            .with(|d| d.as_ref().is_some_and(|d| d.restart_needed))
    });
    let listed = Memo::new(move |_| {
        let query = filter.get();
        let keep = page.editing.get();
        page.data.with(|d| {
            d.as_ref()
                .map(|d| grouped(&d.settings, &query, keep.as_deref()))
                .unwrap_or_default()
        })
    });
    let groups = Memo::new(move |_| {
        listed.with(|l| l.iter().map(|(group, _)| group.clone()).collect::<Vec<_>>())
    });
    let none_listed = Memo::new(move |_| groups.with(Vec::is_empty));
    let count = Memo::new(move |_| {
        let total = page
            .data
            .with(|d| d.as_ref().map_or(0, |d| d.settings.len()));
        let shown = listed.with(|l| l.iter().map(|(_, keys)| keys.len()).sum());
        count_text(shown, total, filter.with(|f| !f.trim().is_empty()))
    });

    let attention = move || {
        if matches!(page.restart.get(), Restart::Waiting(_)) {
            return Some(
                view! {
                    <div class="settings-attention running" role="status">
                        <Status kind=StatusKind::Running label="Restarting…"/>
                        <span class="small dim">"The settings load again once Omni answers."</span>
                    </div>
                }
                .into_any(),
            );
        }
        needs_restart.get().then(|| {
            view! {
                <div class="settings-attention" role="status">
                    <Status kind=StatusKind::Warn label="Saved changes apply after a restart"/>
                    <ConfirmButton
                        label="Restart Omni"
                        confirm_label="Restart now"
                        size=ButtonSize::Sm
                        busy=Signal::derive(move || page.restart.get() == Restart::Asking)
                        disabled=hidden
                        disabled_reason="Trust this browser on the MCP gateway page to restart Omni"
                        title="Interrupts running work; Omni is back in about a minute"
                        on_confirm=ask_restart
                    />
                </div>
            }
            .into_any()
        })
    };

    let list = move || {
        if none_listed.get() {
            let message = Signal::derive(move || {
                let query = filter.get();
                if query.trim().is_empty() {
                    "Omni listed no settings.".to_owned()
                } else {
                    format!("No setting matches “{}”.", query.trim())
                }
            });
            return view! {
                <Panel>
                    <EmptyState
                        compact=true
                        message
                        action=ViewFn::from(move || view! {
                            <Button size=ButtonSize::Sm on_click=Callback::new(move |_| filter.set(String::new()))>
                                "Clear filter"
                            </Button>
                        })
                    />
                </Panel>
            }
            .into_any();
        }
        view! {
            <div class="settings-groups">
                <For
                    each=move || groups.get()
                    key=|group| group.clone()
                    children=move |group| view! { <GroupPanel group listed page/> }
                />
            </div>
        }
        .into_any()
    };

    view! {
        <PageHead title="Settings" lede=LEDE/>
        {move || {
            if loaded.get() {
                return None;
            }
            Some(match load_error.get() {
                Some(e) => view! {
                    <ErrorState
                        title="Settings could not load"
                        raw=e
                        retry=Callback::new(move |()| page.reload())
                        page=true
                    />
                }
                .into_any(),
                None => view! { <SkeletonRows count=8 label="Loading settings"/> }.into_any(),
            })
        }}
        {move || loaded.get().then(|| view! {
            <div class="settings-page">
                {move || load_error.get().map(|e| view! {
                    <InlineNote tone=Tone::Warn role="status">
                        {format!("Refresh failed ({e}). Showing the last loaded state.")}
                    </InlineNote>
                })}
                {move || hidden.get().then(|| view! {
                    <InlineNote tone=Tone::Warn role="status">
                        <span>
                            "Values are hidden because this browser is not trusted. "
                            <Link to="/gateway" class="settings-link">"Trust this browser"</Link>
                            " on the MCP gateway page to see and change them."
                        </span>
                    </InlineNote>
                })}
                {attention}
                <div class="toolbar settings-toolbar">
                    <SearchField
                        value=filter
                        on_input=Callback::new(move |v| filter.set(v))
                        placeholder="Filter by key or group"
                        aria_label="Filter settings"
                        shortcut=true
                    />
                    <span class="small dim num" aria-live="polite">{move || count.get()}</span>
                </div>
                {list}
            </div>
        })}
    }
}

#[component]
fn GroupPanel(
    group: String,
    listed: Memo<Vec<(String, Vec<String>)>>,
    page: Page,
) -> impl IntoView {
    let name = StoredValue::new(group.clone());
    let keys = Memo::new(move |_| {
        listed.with(|l| {
            name.with_value(|name| {
                l.iter()
                    .find(|(g, _)| g == name)
                    .map(|(_, keys)| keys.clone())
                    .unwrap_or_default()
            })
        })
    });
    view! {
        <Panel
            title=group.clone()
            aria_label=format!("{group} settings")
            head_end=ViewFn::from(move || view! { <span class="num">{move || keys.with(Vec::len)}</span> })
        >
            <div class="rows">
                <For
                    each=move || keys.get()
                    key=|key| key.clone()
                    children=move |name| view! { <SettingRow name page/> }
                />
            </div>
        </Panel>
    }
}

fn value_view(value: Shown) -> AnyView {
    match value {
        Shown::Value(text) => {
            let hover = text.clone();
            view! { <span class="mono settings-value" title=hover>{text}</span> }.into_any()
        }
        Shown::Empty => view! { <span class="muted">"empty"</span> }.into_any(),
        Shown::Set => view! { <span class="dim">"set"</span> }.into_any(),
        Shown::NotSet => view! { <span class="muted">"not set"</span> }.into_any(),
    }
}

#[component]
fn SettingRow(name: String, page: Page) -> impl IntoView {
    let name = StoredValue::new(name);
    let item = Memo::new(move |_| {
        page.data.with(|d| {
            name.with_value(|name| {
                d.as_ref()?
                    .settings
                    .iter()
                    .find(|s| s.key == *name)
                    .cloned()
            })
        })
    });
    let editing = Memo::new(move |_| {
        page.editing
            .with(|open| name.with_value(|name| open.as_deref() == Some(name.as_str())))
    });
    let blocker = Memo::new(move |_| {
        let hidden = page
            .data
            .with(|d| d.as_ref().is_some_and(|d| d.values_hidden));
        let restarting = page.restart.get() != Restart::Idle;
        let other_open = page.editing.with(Option::is_some) && !editing.get();
        edit_blocker(hidden, restarting, other_open)
    });
    let form_id = name.with_value(|name| format!("setting-{name}-form"));

    let row = move || {
        let setting = item.get()?;
        let value = shown(&setting);
        let SettingView {
            key,
            secret,
            editable,
            source,
            environment_differs,
            restart_needed,
            updated_at,
            ..
        } = setting;
        let saved = updated_at.map(|at| {
            let at = at as f64;
            view! {
                <span title=format_absolute(at)>{move || format!("saved {}", ago(at, page.now.get()))}</span>
            }
        });
        let edit = editable.then(|| {
            view! {
                <span class="row-end">
                    <Button
                        variant=ButtonVariant::Ghost
                        size=ButtonSize::Sm
                        icon=Icon::Pencil
                        aria_label=format!("Edit {key}")
                        aria_expanded=Signal::derive(move || Some(editing.get().to_string()))
                        aria_controls=form_id.clone()
                        disabled=Signal::derive(move || blocker.with(Option::is_some))
                        disabled_reason=Signal::derive(move || blocker.get().map(str::to_owned))
                        on_click=Callback::new(move |_| {
                            let next = (!editing.get_untracked()).then(|| name.get_value());
                            page.editing.set(next);
                        })
                    >
                        "Edit"
                    </Button>
                </span>
            }
        });
        Some(view! {
            <div class="row settings-row">
                <span class="row-main">
                    <span class="settings-head">
                        <span class="row-title mono settings-key">{key}</span>
                        <Tag title=source_title(source)>{source_label(source)}</Tag>
                        {secret.then(|| view! {
                            <Tag title="Write-only: Omni says whether it is set, never what it is">"secret"</Tag>
                        })}
                        {restart_needed.then(|| view! {
                            <Tag tone=Tone::Warn title="Saved, but the running Omni still uses the value it started with">
                                "restart needed"
                            </Tag>
                        })}
                        {environment_differs.then(|| view! {
                            <span class="settings-dim">
                                <Tag title="The container environment sets a different value; the stored value wins">
                                    "environment differs"
                                </Tag>
                            </span>
                        })}
                    </span>
                    <span class="row-sub settings-sub dot-sep">
                        {value_view(value)}
                        {saved}
                    </span>
                    {(!editable).then(|| view! {
                        <span class="row-sub settings-why" title=READ_ONLY_TITLE>
                            "Set by the container environment only"
                        </span>
                    })}
                </span>
                {edit}
            </div>
        })
    };

    view! {
        {row}
        {move || {
            editing
                .get()
                .then(|| item.get_untracked().map(|setting| view! { <SettingEditor page setting/> }))
        }}
    }
}

#[component]
fn SettingEditor(page: Page, setting: SettingView) -> impl IntoView {
    let secret = setting.secret;
    let stored = setting.source == SettingSource::Stored;
    // A text input drops line breaks, which would corrupt such a value.
    let multiline = setting.value.as_deref().is_some_and(|v| v.contains('\n'));
    let help = value_help(&setting);
    let fallback = clear_action(&setting);
    let draft = RwSignal::new(if secret {
        String::new()
    } else {
        setting.value.clone().unwrap_or_default()
    });
    let error = RwSignal::new(None::<String>);
    let saving = RwSignal::new(false);
    let clearing = RwSignal::new(false);
    let key = StoredValue::new(setting.key);
    let (form_id, input_id) =
        key.with_value(|k| (format!("setting-{k}-form"), format!("setting-{k}")));
    let help_id = format!("{input_id}-help");
    let error_id = format!("{input_id}-error");
    let described = format!("{help_id} {error_id}");

    let input_ref = NodeRef::<Input>::new();
    let area_ref = NodeRef::<Textarea>::new();
    Effect::new(move |_| {
        if let Some(input) = input_ref.get() {
            let _ = input.focus();
        } else if let Some(area) = area_ref.get() {
            let _ = area.focus();
        }
    });

    let close = move || key.with_value(|k| page.close(k));
    let on_input = move |value: String| {
        draft.set(value);
        if error.with_untracked(Option::is_some) {
            error.set(None);
        }
    };
    let on_keydown = move |ev: web_sys::KeyboardEvent| {
        if ev.key() == "Escape" {
            ev.stop_propagation();
            close();
        }
    };

    let save = move || {
        if saving.get_untracked() || clearing.get_untracked() {
            return;
        }
        let value = draft.get_untracked();
        if let Some(problem) = draft_problem(&value, stored) {
            error.set(Some(problem.to_owned()));
            return;
        }
        saving.set(true);
        let key = key.get_value();
        spawn_detached(async move {
            match api::save_setting(&key, &value).await {
                Ok(view) => {
                    page.toast.show(saved_message(&view), ToastKind::Info);
                    page.close(&key);
                    page.apply(view);
                }
                Err(e) => {
                    // A 400 says why the configuration would not validate.
                    if e.status() == Some(400) {
                        error.try_set(Some(e.message().to_owned()));
                    }
                    page.refused(format!("Could not save {key}"), &e);
                }
            }
            saving.try_set(false);
        });
    };

    let clear = Callback::new(move |()| {
        if saving.get_untracked() || clearing.get_untracked() {
            return;
        }
        clearing.set(true);
        let key = key.get_value();
        spawn_detached(async move {
            match api::clear_setting(&key).await {
                Ok(view) => {
                    page.toast.show(cleared_message(&view), ToastKind::Info);
                    page.close(&key);
                    page.apply(view);
                }
                Err(e) => {
                    if e.status() == Some(400) {
                        error.try_set(Some(e.message().to_owned()));
                    }
                    page.refused(format!("Could not remove the stored {key}"), &e);
                }
            }
            clearing.try_set(false);
        });
    });

    let field = if multiline {
        view! {
            <textarea
                id=input_id.clone()
                node_ref=area_ref
                class="textarea mono"
                rows="4"
                autocomplete="off"
                autocapitalize="none"
                spellcheck="false"
                aria-describedby=described
                aria-invalid=move || error.with(Option::is_some).then_some("true")
                prop:value=move || draft.get()
                on:input=move |ev| on_input(event_target_value(&ev))
                on:keydown=on_keydown
            ></textarea>
        }
        .into_any()
    } else {
        view! {
            <input
                id=input_id.clone()
                node_ref=input_ref
                class="input mono"
                type=if secret { "password" } else { "text" }
                autocomplete=if secret { "new-password" } else { "off" }
                autocapitalize="none"
                spellcheck="false"
                aria-describedby=described
                aria-invalid=move || error.with(Option::is_some).then_some("true")
                prop:value=move || draft.get()
                on:input=move |ev| on_input(event_target_value(&ev))
                on:keydown=on_keydown
            />
        }
        .into_any()
    };

    view! {
        <form
            id=form_id
            class="panel-body settings-form"
            aria-label=key.with_value(|k| format!("Edit {k}"))
            novalidate=true
            on:submit=move |event| {
                event.prevent_default();
                save();
            }
        >
            <div class="field">
                <label class="field-label" for=input_id>{if secret { "New value" } else { "Value" }}</label>
                {field}
                <p class="field-help" id=help_id>{help}</p>
                {move || error.get().map(|e| view! { <p class="field-error" id=error_id.clone()>{e}</p> })}
            </div>
            <div class="cluster settings-actions">
                <Button submit=true busy=saving>"Save"</Button>
                <Button variant=ButtonVariant::Ghost on_click=Callback::new(move |_| close())>"Cancel"</Button>
                {stored.then(|| view! {
                    <span class="settings-clear">
                        <ConfirmButton
                            label=fallback.label
                            confirm_label=fallback.confirm
                            variant=ButtonVariant::Ghost
                            destructive=fallback.loses_value
                            busy=clearing
                            title=fallback.title
                            context=key.get_value()
                            on_confirm=clear
                        />
                    </span>
                })}
            </div>
        </form>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setting(key: &str, group: &str) -> SettingView {
        SettingView {
            key: key.to_owned(),
            group: group.to_owned(),
            secret: false,
            editable: true,
            source: SettingSource::Environment,
            set: true,
            value: Some("x".to_owned()),
            environment_set: true,
            environment_differs: false,
            restart_needed: false,
            updated_at: None,
        }
    }

    fn all() -> Vec<SettingView> {
        vec![
            setting("SONARR_URL", "Media"),
            setting("PLEX_URL", "Media"),
            setting("PUSHOVER_TOKEN", "Notifications"),
            setting("EMAIL_HOST", "Email and calendar"),
            setting("PLEX_TOKEN", "Media"),
        ]
    }

    fn view(booted_at: Ms) -> SettingsView {
        SettingsView {
            settings: all(),
            restart_needed: true,
            values_hidden: false,
            booted_at,
        }
    }

    #[test]
    fn groups_sort_by_name_and_keys_within() {
        assert_eq!(
            grouped(&all(), "", None),
            [
                (
                    "Email and calendar".to_owned(),
                    vec!["EMAIL_HOST".to_owned()]
                ),
                (
                    "Media".to_owned(),
                    vec![
                        "PLEX_TOKEN".to_owned(),
                        "PLEX_URL".to_owned(),
                        "SONARR_URL".to_owned()
                    ]
                ),
                (
                    "Notifications".to_owned(),
                    vec!["PUSHOVER_TOKEN".to_owned()]
                ),
            ]
        );
    }

    #[test]
    fn filter_matches_key_or_group_by_every_term() {
        let keys = |query: &str| -> Vec<String> {
            grouped(&all(), query, None)
                .into_iter()
                .flat_map(|(_, keys)| keys)
                .collect()
        };
        assert_eq!(
            keys("plex url"),
            ["PLEX_URL"],
            "terms in any order and case"
        );
        assert_eq!(keys("  token "), ["PLEX_TOKEN", "PUSHOVER_TOKEN"]);
        assert_eq!(keys("calendar"), ["EMAIL_HOST"], "the group matches");
        assert_eq!(
            keys("media token"),
            ["PLEX_TOKEN"],
            "group and key terms combine"
        );
        assert!(keys("radarr").is_empty());
        assert_eq!(keys("").len(), 5);
    }

    #[test]
    fn an_open_form_stays_listed() {
        assert_eq!(
            grouped(&all(), "pushover", Some("PLEX_URL")),
            [
                ("Media".to_owned(), vec!["PLEX_URL".to_owned()]),
                (
                    "Notifications".to_owned(),
                    vec!["PUSHOVER_TOKEN".to_owned()]
                ),
            ]
        );
    }

    #[test]
    fn values_show_only_what_the_server_sent() {
        let mut s = setting("PLEX_URL", "Media");
        assert_eq!(shown(&s), Shown::Value("x".to_owned()));
        s.value = Some(String::new());
        assert_eq!(shown(&s), Shown::Empty);
        s.value = None;
        assert_eq!(shown(&s), Shown::Set, "a secret or a hidden value");
        s.set = false;
        assert_eq!(shown(&s), Shown::NotSet);
    }

    #[test]
    fn edit_says_why_it_is_disabled() {
        assert!(edit_blocker(true, false, false).is_some_and(|r| r.contains("Trust")));
        assert_eq!(edit_blocker(false, true, true), Some("Omni is restarting"));
        assert_eq!(
            edit_blocker(false, false, true),
            Some("Finish the open setting first")
        );
        assert_eq!(edit_blocker(false, false, false), None);
    }

    #[test]
    fn empty_values_are_not_sent() {
        assert_eq!(draft_problem("http://plex:32400", false), None);
        assert_eq!(draft_problem("  ", false), Some("Type a value."));
        assert!(draft_problem("", true).is_some_and(|p| p.contains("environment value")));
    }

    #[test]
    fn messages_say_when_a_change_applies() {
        let mut s = setting("PLEX_URL", "Media");
        s.source = SettingSource::Stored;
        s.restart_needed = true;
        assert_eq!(
            saved_message(&s),
            "Saved PLEX_URL. It applies after a restart."
        );
        s.restart_needed = false;
        assert_eq!(
            saved_message(&s),
            "Saved PLEX_URL. Omni already uses this value."
        );
        s.source = SettingSource::Default;
        s.restart_needed = true;
        assert_eq!(
            cleared_message(&s),
            "PLEX_URL uses the built-in default after a restart."
        );
        s.source = SettingSource::Environment;
        s.restart_needed = false;
        assert_eq!(cleared_message(&s), "PLEX_URL uses the environment value.");
    }

    #[test]
    fn a_restart_is_over_once_a_new_process_answers() {
        let before = 1_764_000_000_000;
        assert!(!restarted(before, &view(before)), "still the old process");
        assert!(
            restarted(before, &view(before + 42_000)),
            "a new process, even while a saved value did not apply"
        );
    }

    #[test]
    fn removing_a_stored_value_names_the_fallback() {
        let mut s = setting("PLEX_URL", "Media");
        s.source = SettingSource::Stored;
        let env = clear_action(&s);
        assert_eq!(env.label, "Use environment value");
        assert!(!env.loses_value);
        s.environment_set = false;
        let default = clear_action(&s);
        assert_eq!(default.label, "Use default");
        assert!(
            !default.loses_value,
            "the value is visible, so nothing is lost"
        );
        s.secret = true;
        let secret = clear_action(&s);
        assert_eq!(secret.label, "Use default");
        assert!(secret.loses_value);
        assert!(secret.confirm.contains("Lose"));
        s.environment_set = true;
        assert!(
            !clear_action(&s).loses_value,
            "the environment still has it"
        );
    }

    #[test]
    fn counts_name_the_filter() {
        assert_eq!(count_text(140, 140, false), "140 settings");
        assert_eq!(count_text(1, 1, false), "1 setting");
        assert_eq!(count_text(3, 140, true), "3 of 140");
        assert_eq!(source_label(SettingSource::Environment), "Environment");
    }
}
