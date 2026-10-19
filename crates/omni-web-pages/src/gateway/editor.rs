//! Forms: adding an integration, its settings, write-only credentials, the
//! OAuth connection and deletion. Credential values never come back from
//! the server; inputs start empty and saving replaces what is stored.

use leptos::prelude::*;
use omni_api::gateway::{AuthKind, BundledServerView, IntegrationStatus, TransportKind};
use omni_web_kit::api;
use omni_web_kit::components::{
    Button, ButtonSize, ButtonVariant, ConfirmButton, Icon, InlineNote, Inspector, SegOption,
    Segmented, Status, StatusKind, Tag, ToastKind, Tone,
};
use omni_web_kit::task::spawn_detached;
use omni_web_kit::utils::format::{format_absolute, format_relative_at};

use super::Gateway;
use super::detail::DetailTab;
use super::logic::{
    self, CreateDraft, CreateProblems, CredentialDraft, MAX_ID_CHARS, MAX_NAME_CHARS, MAX_TIMEOUT,
    MIN_TIMEOUT, SecretKind, SettingsDraft,
};

fn auth_options(transport: TransportKind) -> Vec<SegOption<AuthKind>> {
    logic::auth_kinds(transport)
        .iter()
        .map(|kind| SegOption::new(*kind, logic::auth_label(*kind)))
        .collect()
}

fn bundled_untracked(gw: Gateway) -> Vec<BundledServerView> {
    gw.overview.with_untracked(|o| {
        o.as_ref()
            .map(|o| o.bundled_servers.clone())
            .unwrap_or_default()
    })
}

// ----- credential inputs -----

/// Header or variable rows over a [`CredentialDraft`]. `stored` names get a
/// "stored" tag; their inputs replace the stored value.
#[component]
pub(super) fn CredentialFields(
    draft: RwSignal<CredentialDraft>,
    auth: AuthKind,
    #[prop(into)] stored: Signal<Vec<String>>,
    id_prefix: &'static str,
) -> impl IntoView {
    let keys = Memo::new(move |_| draft.with(|d| d.rows.iter().map(|r| r.key).collect::<Vec<_>>()));
    let has_bearer = Memo::new(move |_| draft.with(CredentialDraft::has_bearer));
    view! {
        <div class="gw-creds">
            <For
                each=move || keys.get()
                key=|key| *key
                children=move |key| view! { <SecretInput draft key stored id_prefix/> }
            />
            <div class="cluster">
                {match auth {
                    AuthKind::Env => view! {
                        <Button size=ButtonSize::Sm variant=ButtonVariant::Ghost icon=Icon::Plus on_click=Callback::new(move |_| draft.update(CredentialDraft::add_env))>
                            "Add variable"
                        </Button>
                    }
                    .into_any(),
                    _ => view! {
                        {move || (!has_bearer.get()).then(|| view! {
                            <Button size=ButtonSize::Sm variant=ButtonVariant::Ghost icon=Icon::Plus on_click=Callback::new(move |_| draft.update(CredentialDraft::add_bearer))>
                                "Add bearer token"
                            </Button>
                        })}
                        <Button size=ButtonSize::Sm variant=ButtonVariant::Ghost icon=Icon::Plus on_click=Callback::new(move |_| draft.update(CredentialDraft::add_header))>
                            "Add header"
                        </Button>
                    }
                    .into_any(),
                }}
            </div>
        </div>
    }
}

#[component]
fn SecretInput(
    draft: RwSignal<CredentialDraft>,
    key: u32,
    stored: Signal<Vec<String>>,
    id_prefix: &'static str,
) -> impl IntoView {
    let Some((kind, initial_name)) = draft.with_untracked(|d| {
        d.rows
            .iter()
            .find(|r| r.key == key)
            .map(|r| (r.kind, r.name.clone()))
    }) else {
        return ().into_any();
    };
    let name = Memo::new(move |_| {
        draft.with(|d| {
            d.rows
                .iter()
                .find(|r| r.key == key)
                .map(|r| r.name.clone())
                .unwrap_or_default()
        })
    });
    let is_stored = Memo::new(move |_| {
        let name = name.get();
        let name = name.trim();
        !name.is_empty()
            && stored.with(|s| match kind {
                SecretKind::Env { .. } => s.iter().any(|n| n == name),
                _ => s.iter().any(|n| n.eq_ignore_ascii_case(name)),
            })
    });
    let name_id = format!("{id_prefix}-name-{key}");
    let value_id = format!("{id_prefix}-value-{key}");
    let removable = !matches!(kind, SecretKind::Env { required: true });
    let what = match kind {
        SecretKind::Bearer => "the bearer token".to_owned(),
        _ if initial_name.is_empty() => "this row".to_owned(),
        _ => initial_name.clone(),
    };
    let name_cell = match kind {
        SecretKind::Bearer => view! {
            <span class="gw-cred-name">
                <label class="field-label" for=value_id.clone()>"Bearer token"</label>
                <span class="small muted">"Sent as Authorization: Bearer"</span>
            </span>
        }
        .into_any(),
        SecretKind::Env { required: true } => view! {
            <span class="gw-cred-name">
                <label class="field-label mono" for=value_id.clone()>{initial_name.clone()}</label>
                <span class="small muted">"Required by the server"</span>
            </span>
        }
        .into_any(),
        SecretKind::Header | SecretKind::Env { required: false } => {
            let placeholder = if kind == SecretKind::Header {
                "Header name"
            } else {
                "VARIABLE_NAME"
            };
            view! {
                <span class="gw-cred-name">
                    <label class="sr-only" for=name_id.clone()>
                        {if kind == SecretKind::Header { "Header name" } else { "Variable name" }}
                    </label>
                    <input
                        id=name_id.clone()
                        class="input mono"
                        type="text"
                        autocomplete="off"
                        autocapitalize="off"
                        spellcheck="false"
                        placeholder=placeholder
                        prop:value=move || name.get()
                        on:input=move |ev| draft.update(|d| d.set_name(key, event_target_value(&ev)))
                    />
                </span>
            }
            .into_any()
        }
    };
    let value_label = Signal::derive(move || match kind {
        SecretKind::Bearer => "Bearer token value".to_owned(),
        _ => format!("Value for {}", name.get().trim()),
    });
    view! {
        <div class="gw-cred">
            {name_cell}
            <span class="gw-cred-value">
                <input
                    id=value_id.clone()
                    class="input mono"
                    type="password"
                    autocomplete="new-password"
                    autocapitalize="off"
                    spellcheck="false"
                    aria-label=move || value_label.get()
                    placeholder=move || if is_stored.get() { "Type to replace" } else { "Value" }
                    prop:value=move || {
                        draft.with(|d| d.rows.iter().find(|r| r.key == key).map(|r| r.value.clone()).unwrap_or_default())
                    }
                    on:input=move |ev| draft.update(|d| d.set_value(key, event_target_value(&ev)))
                />
                {move || is_stored.get().then(|| view! { <Tag title="A value is stored; it is never shown">"stored"</Tag> })}
            </span>
            {removable.then(|| view! {
                <Button
                    size=ButtonSize::Sm
                    variant=ButtonVariant::Ghost
                    icon=Icon::Trash
                    icon_only=true
                    aria_label=format!("Remove {what}")
                    title=format!("Remove {what}")
                    on_click=Callback::new(move |_| draft.update(|d| d.remove(key)))
                />
            })}
        </div>
    }
    .into_any()
}

// ----- add -----

#[component]
pub(super) fn AddIntegration(gw: Gateway, on_close: Callback<()>) -> impl IntoView {
    let bundled = Memo::new(move |_| {
        gw.overview.with(|o| {
            o.as_ref()
                .map(|o| o.bundled_servers.clone())
                .unwrap_or_default()
        })
    });
    let existing = Memo::new(move |_| {
        gw.overview.with(|o| {
            o.as_ref()
                .map(|o| {
                    o.integrations
                        .iter()
                        .map(|i| i.id.clone())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        })
    });
    let draft = RwSignal::new(CreateDraft::default());
    let creds = RwSignal::new(CredentialDraft::default());
    let attempted = RwSignal::new(false);
    let saving = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);

    let problems = Memo::new(move |_| {
        draft.with(|d| existing.with(|e| bundled.with(|b| logic::create_problems(d, e, b))))
    });
    let shown = Memo::new(move |_| {
        if attempted.get() {
            problems.get()
        } else {
            CreateProblems::default()
        }
    });
    // New credential rows whenever the auth kind or bundled server changes.
    let shape = Memo::new(move |_| draft.with(|d| (d.transport, d.auth, d.server.clone())));
    Effect::new(move |_| {
        let (_, auth, _) = shape.get();
        let required =
            draft.with_untracked(|d| bundled.with_untracked(|b| d.required_env(b).to_vec()));
        creds.set(CredentialDraft::for_auth(auth, &required, &[]));
    });
    let transport = Memo::new(move |_| draft.with(|d| d.transport));
    let auth = Memo::new(move |_| draft.with(|d| d.auth));

    let save = move || {
        if saving.get_untracked() {
            return;
        }
        attempted.set(true);
        if !problems.get_untracked().is_empty() {
            return;
        }
        let current = draft.get_untracked();
        let credentials = match current.auth {
            AuthKind::Headers | AuthKind::Env
                if creds.with_untracked(CredentialDraft::has_input) =>
            {
                match creds.with_untracked(|c| c.build(current.auth)) {
                    Ok(update) => Some(update),
                    Err(e) => {
                        error.set(Some(e));
                        return;
                    }
                }
            }
            _ => None,
        };
        error.set(None);
        saving.set(true);
        let body = logic::build_create(&current);
        spawn_detached(async move {
            let created = match api::create_gateway_integration(&body).await {
                Ok(detail) => detail,
                Err(e) => {
                    error.try_set(Some(e.message().to_owned()));
                    saving.try_set(false);
                    return;
                }
            };
            let id = created.summary.id.clone();
            let name = created.summary.name.clone();
            let mut latest = created;
            if let Some(update) = credentials {
                match api::put_gateway_credentials(&id, &update).await {
                    Ok(detail) => latest = detail,
                    Err(e) => {
                        gw.toast.show(
                            format!(
                                "Added {name}, but its credentials were not saved: {}",
                                e.message()
                            ),
                            ToastKind::Error,
                        );
                        gw.apply(latest);
                        gw.open(id, DetailTab::Settings);
                        return;
                    }
                }
            }
            let oauth = latest.summary.auth == AuthKind::OAuth;
            let load = !oauth
                && latest.summary.enabled
                && latest.summary.status == IntegrationStatus::Pending;
            gw.apply(latest);
            gw.toast.show(format!("Added {name}"), ToastKind::Info);
            gw.open(
                id.clone(),
                if oauth {
                    DetailTab::Settings
                } else {
                    DetailTab::Tools
                },
            );
            if oauth {
                gw.connect(id);
            } else if load {
                gw.refresh(id);
            }
        });
    };

    let actions = ViewFn::from(move || {
        view! {
            <Button
                variant=ButtonVariant::Primary
                icon=Icon::Check
                busy=saving
                on_click=Callback::new(move |_| save())
            >
                {move || if auth.get() == AuthKind::OAuth { "Add and connect" } else { "Add integration" }}
            </Button>
            <Button variant=ButtonVariant::Ghost on_click=Callback::new(move |_| on_close.run(()))>"Cancel"</Button>
        }
    });
    let field_error = move |pick: fn(&CreateProblems) -> Option<String>| {
        move || {
            shown
                .with(pick)
                .map(|e| view! { <p class="field-error">{e}</p> })
        }
    };
    let invalid = move |pick: fn(&CreateProblems) -> Option<String>| {
        move || shown.with(|p| pick(p).is_some()).then_some("true")
    };

    view! {
        <Inspector title="Add integration" actions on_close>
            <form
                class="gw-form"
                on:submit=move |event| {
                    event.prevent_default();
                    save();
                }
            >
                <div class="inspector-section stack">
                    <div class="field">
                        <label class="field-label" for="gw-new-name">"Name"</label>
                        <input
                            id="gw-new-name"
                            class="input"
                            type="text"
                            autocomplete="off"
                            maxlength=MAX_NAME_CHARS.to_string()
                            placeholder="Linear"
                            aria-invalid=invalid(|p| p.name.clone())
                            prop:value=move || draft.with(|d| d.name.clone())
                            on:input=move |ev| draft.update(|d| d.set_name(event_target_value(&ev)))
                        />
                        {field_error(|p| p.name.clone())}
                    </div>
                    <div class="field">
                        <label class="field-label" for="gw-new-id">"Id"</label>
                        <input
                            id="gw-new-id"
                            class="input mono"
                            type="text"
                            autocomplete="off"
                            autocapitalize="off"
                            spellcheck="false"
                            maxlength=MAX_ID_CHARS.to_string()
                            placeholder="linear"
                            aria-invalid=invalid(|p| p.id.clone())
                            aria-describedby="gw-new-id-help"
                            prop:value=move || draft.with(|d| d.id.clone())
                            on:input=move |ev| draft.update(|d| d.set_id(event_target_value(&ev)))
                        />
                        <p class="field-help" id="gw-new-id-help">
                            "Agents see its tools as "
                            <span class="mono">{move || format!("{}__<tool>", draft.with(|d| if d.id.is_empty() { "id".to_owned() } else { d.id.clone() }))}</span>
                            ". Lowercase letters, digits and _; it cannot change later."
                        </p>
                        {field_error(|p| p.id.clone())}
                    </div>
                </div>
                <div class="inspector-section stack">
                    <h3>"Server"</h3>
                    {move || (!bundled.with(Vec::is_empty)).then(|| view! {
                        <Segmented
                            options=Signal::derive(|| vec![
                                SegOption::new(TransportKind::Http, logic::transport_label(TransportKind::Http)),
                                SegOption::new(TransportKind::Stdio, logic::transport_label(TransportKind::Stdio)),
                            ])
                            value=transport
                            on_change=Callback::new(move |t| {
                                let servers = bundled.get_untracked();
                                draft.update(|d| d.set_transport(t, &servers));
                            })
                            aria_label="Transport"
                            small=true
                        />
                    })}
                    {move || match transport.get() {
                        TransportKind::Http => view! {
                            <div class="field">
                                <label class="field-label" for="gw-new-url">"MCP URL"</label>
                                <input
                                    id="gw-new-url"
                                    class="input mono"
                                    type="url"
                                    inputmode="url"
                                    autocomplete="off"
                                    autocapitalize="off"
                                    spellcheck="false"
                                    placeholder="https://mcp.example.com/mcp"
                                    aria-invalid=invalid(|p| p.url.clone())
                                    prop:value=move || draft.with(|d| d.url.clone())
                                    on:input=move |ev| draft.update(|d| d.url = event_target_value(&ev))
                                />
                                <p class="field-help">"The server's Streamable HTTP endpoint. HTTPS only."</p>
                                {field_error(|p| p.url.clone())}
                            </div>
                        }
                        .into_any(),
                        TransportKind::Stdio => view! { <BundledPicker draft bundled shown/> }.into_any(),
                    }}
                </div>
                <div class="inspector-section stack">
                    <h3>"Authentication"</h3>
                    <Segmented
                        options=Signal::derive(move || auth_options(transport.get()))
                        value=auth
                        on_change=Callback::new(move |kind| draft.update(|d| d.auth = kind))
                        aria_label="Authentication"
                        small=true
                    />
                    <p class="field-help">{move || logic::auth_help(auth.get())}</p>
                    {move || match auth.get() {
                        kind @ (AuthKind::Headers | AuthKind::Env) => view! {
                            <CredentialFields draft=creds auth=kind stored=Signal::stored(Vec::new()) id_prefix="gw-new-cred"/>
                            <p class="field-help">"Optional now; you can add them after the integration exists."</p>
                        }
                        .into_any(),
                        AuthKind::OAuth => view! {
                            <p class="field-help">"After adding, Omni opens the server's sign-in page and returns here."</p>
                        }
                        .into_any(),
                        AuthKind::None => ().into_any(),
                    }}
                </div>
                <div class="inspector-section field">
                    <label class="field-label" for="gw-new-timeout">"Timeout (seconds)"</label>
                    <input
                        id="gw-new-timeout"
                        class="input num gw-timeout"
                        type="number"
                        inputmode="numeric"
                        min=MIN_TIMEOUT.to_string()
                        max=MAX_TIMEOUT.to_string()
                        step="1"
                        aria-invalid=invalid(|p| p.timeout.clone())
                        prop:value=move || draft.with(|d| d.timeout.clone())
                        on:input=move |ev| draft.update(|d| d.timeout = event_target_value(&ev))
                    />
                    <p class="field-help">{format!("How long one request may take, {MIN_TIMEOUT} to {MAX_TIMEOUT}.")}</p>
                    {field_error(|p| p.timeout.clone())}
                </div>
                {move || error.get().map(|e| view! {
                    <div class="inspector-section">
                        <p class="field-error" role="alert">{e}</p>
                    </div>
                })}
                // Enter in a text field submits through this hidden button.
                <button type="submit" class="sr-only" tabindex="-1" aria-hidden="true"></button>
            </form>
        </Inspector>
    }
}

#[component]
fn BundledPicker(
    draft: RwSignal<CreateDraft>,
    bundled: Memo<Vec<BundledServerView>>,
    shown: Memo<CreateProblems>,
) -> impl IntoView {
    let server = Memo::new(move |_| draft.with(|d| d.server.clone()));
    let chosen = Memo::new(move |_| {
        let id = server.get();
        bundled.with(|b| b.iter().find(|s| s.id == id).cloned())
    });
    view! {
        <div class="field">
            <label class="field-label" for="gw-new-server">"Bundled server"</label>
            <select
                id="gw-new-server"
                class="select"
                aria-invalid=move || shown.with(|p| p.server.is_some()).then_some("true")
                on:change=move |ev| {
                    let servers = bundled.get_untracked();
                    draft.update(|d| d.set_server(event_target_value(&ev), &servers));
                }
            >
                <option value="" disabled=true prop:selected=move || server.with(String::is_empty)>"Choose a server"</option>
                {move || bundled.get().into_iter().map(|b| {
                    let id = b.id.clone();
                    let label = if b.available { b.name.clone() } else { format!("{} (not installed)", b.name) };
                    view! {
                        <option value=b.id.clone() disabled=!b.available prop:selected=move || server.with(|s| *s == id)>
                            {label}
                        </option>
                    }
                }).collect_view()}
            </select>
            {move || chosen.get().map(|b| view! {
                <p class="field-help">
                    <span class="mono">{b.package.clone()}</span>
                    {(!b.env.is_empty()).then(|| format!(" · needs {}", b.env.join(", ")))}
                </p>
            })}
            {move || shown.with(|p| p.server.clone()).map(|e| view! { <p class="field-error">{e}</p> })}
        </div>
    }
}

// ----- settings -----

#[component]
pub(super) fn SettingsForm(gw: Gateway, draft: RwSignal<Option<SettingsDraft>>) -> impl IntoView {
    let base = Memo::new(move |_| {
        gw.detail
            .with(|d| d.as_ref().map(|d| (d.summary.clone(), d.config.clone())))
    });
    if draft.with_untracked(Option::is_none)
        && let Some((summary, config)) = base.get_untracked()
    {
        draft.set(Some(SettingsDraft::of(&summary, &config)));
    }
    let saving = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let patch = Memo::new(move |_| {
        base.with(|b| {
            draft.with(|d| match (b, d) {
                (Some((s, c)), Some(d)) => Some(logic::build_patch(s, c, d)),
                _ => None,
            })
        })
    });
    let unchanged = Signal::derive(move || {
        patch.with(|p| matches!(p, Some(Ok(p)) if *p == Default::default()))
    });
    let problem =
        Memo::new(move |_| patch.with(|p| p.as_ref().and_then(|p| p.as_ref().err().cloned())));
    let clears = Memo::new(move |_| {
        base.with(|b| {
            draft.with(|d| match (b, d) {
                (Some((s, c)), Some(d)) => logic::clears_credentials(s, c, d),
                _ => None,
            })
        })
    });
    let transport = Memo::new(move |_| {
        base.with(|b| b.as_ref().map_or(TransportKind::Http, |(s, _)| s.transport))
    });
    let field = move |f: fn(&SettingsDraft) -> String| {
        move || draft.with(|d| d.as_ref().map(f).unwrap_or_default())
    };
    let edit = move |f: fn(&mut SettingsDraft, String)| {
        move |ev: web_sys::Event| {
            let value = event_target_value(&ev);
            draft.update(|d| {
                if let Some(d) = d {
                    f(d, value);
                }
            });
        }
    };
    let save = move || {
        let Some(Ok(patch)) = patch.get_untracked() else {
            return;
        };
        if saving.get_untracked() || patch == Default::default() {
            return;
        }
        let Some(id) = base.with_untracked(|b| b.as_ref().map(|(s, _)| s.id.clone())) else {
            return;
        };
        saving.set(true);
        error.set(None);
        spawn_detached(async move {
            match api::update_gateway_integration(&id, &patch).await {
                Ok(detail) => {
                    draft.try_set(Some(SettingsDraft::of(&detail.summary, &detail.config)));
                    gw.toast
                        .show(format!("Saved {}", detail.summary.name), ToastKind::Info);
                    gw.apply(detail);
                }
                Err(e) => {
                    error.try_set(Some(e.message().to_owned()));
                }
            }
            saving.try_set(false);
        });
    };
    let reset = move |_| {
        if let Some((summary, config)) = base.get_untracked() {
            draft.set(Some(SettingsDraft::of(&summary, &config)));
        }
        error.set(None);
    };
    view! {
        <form
            class="inspector-section stack gw-form"
            on:submit=move |event| {
                event.prevent_default();
                save();
            }
        >
            <h3>"Settings"</h3>
            <div class="field">
                <label class="field-label" for="gw-edit-name">"Name"</label>
                <input
                    id="gw-edit-name"
                    class="input"
                    type="text"
                    autocomplete="off"
                    maxlength=MAX_NAME_CHARS.to_string()
                    prop:value=field(|d| d.name.clone())
                    on:input=edit(|d, v| d.name = v)
                />
            </div>
            {move || match transport.get() {
                TransportKind::Http => view! {
                    <div class="field">
                        <label class="field-label" for="gw-edit-url">"MCP URL"</label>
                        <input
                            id="gw-edit-url"
                            class="input mono"
                            type="url"
                            inputmode="url"
                            autocomplete="off"
                            autocapitalize="off"
                            spellcheck="false"
                            prop:value=field(|d| d.url.clone())
                            on:input=edit(|d, v| d.url = v)
                        />
                    </div>
                }
                .into_any(),
                TransportKind::Stdio => view! {
                    <p class="field-help">
                        "Runs the bundled server "
                        <span class="mono">{move || base.with(|b| b.as_ref().map(|(s, _)| s.endpoint.clone()).unwrap_or_default())}</span>
                        ". Add a new integration to use another one."
                    </p>
                }
                .into_any(),
            }}
            <div class="field">
                <span class="field-label">"Authentication"</span>
                <Segmented
                    options=Signal::derive(move || auth_options(transport.get()))
                    value=Signal::derive(move || draft.with(|d| d.as_ref().map_or(AuthKind::None, |d| d.auth)))
                    on_change=Callback::new(move |kind| draft.update(|d| {
                        if let Some(d) = d {
                            d.auth = kind;
                        }
                    }))
                    aria_label="Authentication"
                    small=true
                />
            </div>
            <div class="field">
                <label class="field-label" for="gw-edit-timeout">"Timeout (seconds)"</label>
                <input
                    id="gw-edit-timeout"
                    class="input num gw-timeout"
                    type="number"
                    inputmode="numeric"
                    min=MIN_TIMEOUT.to_string()
                    max=MAX_TIMEOUT.to_string()
                    step="1"
                    prop:value=field(|d| d.timeout.clone())
                    on:input=edit(|d, v| d.timeout = v)
                />
            </div>
            {move || match clears.get() {
                Some(why) => view! {
                    <InlineNote tone=Tone::Warn>
                        {format!("Saving clears the stored credentials: {why}.")}
                    </InlineNote>
                }
                .into_any(),
                None => view! {
                    <p class="field-help">"Changing the URL's origin or the auth kind clears stored credentials."</p>
                }
                .into_any(),
            }}
            {move || problem.get().map(|e| view! { <p class="field-error">{e}</p> })}
            {move || error.get().map(|e| view! { <p class="field-error" role="alert">{e}</p> })}
            <div class="cluster">
                <Button
                    submit=true
                    busy=saving
                    disabled=Signal::derive(move || unchanged.get() || problem.with(Option::is_some))
                    disabled_reason=Signal::derive(move || if problem.with(Option::is_some) {
                        "Fix the highlighted problem first".to_owned()
                    } else {
                        "No change to save".to_owned()
                    })
                >
                    "Save settings"
                </Button>
                {move || (!unchanged.get()).then(|| view! {
                    <Button variant=ButtonVariant::Ghost on_click=Callback::new(reset)>"Discard"</Button>
                })}
            </div>
        </form>
    }
}

/// The credentials for the saved auth kind.
#[component]
pub(super) fn CredentialsSection(gw: Gateway) -> impl IntoView {
    let auth = Memo::new(move |_| {
        gw.detail
            .with(|d| d.as_ref().map_or(AuthKind::None, |d| d.summary.auth))
    });
    move || {
        match auth.get() {
        AuthKind::None => view! {
            <section class="inspector-section">
                <h3>"Credentials"</h3>
                <p class="small dim">"None: requests go out without authentication. Choose an auth kind in Settings to add some."</p>
            </section>
        }
        .into_any(),
        AuthKind::OAuth => view! { <OAuthSection gw/> }.into_any(),
        kind @ (AuthKind::Headers | AuthKind::Env) => view! { <SecretSection gw auth=kind/> }.into_any(),
    }
    }
}

#[component]
fn SecretSection(gw: Gateway, auth: AuthKind) -> impl IntoView {
    let stored = Memo::new(move |_| {
        gw.detail.with(|d| {
            d.as_ref()
                .map(|d| logic::stored_names(auth, &d.config))
                .unwrap_or_default()
        })
    });
    let complete = Memo::new(move |_| {
        gw.detail
            .with(|d| d.as_ref().is_none_or(|d| d.config.credentials_set))
    });
    let required = move || {
        let bundled = bundled_untracked(gw);
        gw.detail.with_untracked(|d| {
            d.as_ref()
                .and_then(|d| d.config.bundled.clone())
                .and_then(|id| bundled.into_iter().find(|b| b.id == id))
                .map(|b| b.env)
                .unwrap_or_default()
        })
    };
    let draft = RwSignal::new(CredentialDraft::for_auth(
        auth,
        &required(),
        &stored.get_untracked(),
    ));
    // Saving or clearing changes the stored names: start the rows again.
    Effect::new(move |previous: Option<Vec<String>>| {
        let names = stored.get();
        if previous.is_some_and(|p| p != names) {
            draft.set(CredentialDraft::for_auth(auth, &required(), &names));
        }
        names
    });
    let saving = RwSignal::new(false);
    let clearing = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let has_input = Memo::new(move |_| draft.with(CredentialDraft::has_input));
    let id = move || {
        gw.detail.with_untracked(|d| {
            d.as_ref()
                .map(|d| (d.summary.id.clone(), d.summary.name.clone()))
        })
    };
    let noun = if auth == AuthKind::Env {
        "variable"
    } else {
        "header"
    };

    let save = move || {
        if saving.get_untracked() {
            return;
        }
        let update = match draft.with_untracked(|d| d.build(auth)) {
            Ok(update) => update,
            Err(e) => {
                error.set(Some(e));
                return;
            }
        };
        let Some((id, name)) = id() else { return };
        error.set(None);
        saving.set(true);
        spawn_detached(async move {
            match api::put_gateway_credentials(&id, &update).await {
                Ok(detail) => {
                    draft.try_update(CredentialDraft::clear_values);
                    gw.toast
                        .show(format!("Saved credentials for {name}"), ToastKind::Info);
                    gw.apply(detail);
                }
                Err(e) => {
                    error.try_set(Some(e.message().to_owned()));
                }
            }
            saving.try_set(false);
        });
    };
    let clear = Callback::new(move |()| {
        let Some((id, name)) = id() else { return };
        clearing.set(true);
        error.set(None);
        spawn_detached(async move {
            match api::clear_gateway_credentials(&id).await {
                Ok(detail) => {
                    gw.toast
                        .show(format!("Cleared credentials for {name}"), ToastKind::Info);
                    gw.apply(detail);
                }
                Err(e) => {
                    error.try_set(Some(e.message().to_owned()));
                }
            }
            clearing.try_set(false);
        });
    });

    view! {
        <form
            class="inspector-section stack gw-form"
            on:submit=move |event| {
                event.prevent_default();
                save();
            }
        >
            <h3>"Credentials"</h3>
            <div class="cluster gw-stored">
                <span class="small muted">"Stored"</span>
                {move || {
                    let names = stored.get();
                    if names.is_empty() {
                        view! { <span class="small dim">"Nothing yet"</span> }.into_any()
                    } else {
                        names
                            .into_iter()
                            .map(|n| view! { <Tag>{n}</Tag> })
                            .collect_view()
                            .into_any()
                    }
                }}
            </div>
            {move || (!complete.get()).then(|| view! {
                <InlineNote tone=Tone::Warn>"Missing credentials: the integration cannot connect until every one is set."</InlineNote>
            })}
            <CredentialFields draft auth stored id_prefix="gw-cred"/>
            <p class="field-help">
                {format!("Saving replaces every stored {noun}. Values are write-only: Omni never shows them, so enter each one again.")}
            </p>
            {move || error.get().map(|e| view! { <p class="field-error" role="alert">{e}</p> })}
            <div class="cluster">
                <Button
                    submit=true
                    busy=saving
                    disabled=Signal::derive(move || !has_input.get())
                    disabled_reason="Enter a value first"
                >
                    {move || if stored.with(Vec::is_empty) { "Save credentials" } else { "Replace credentials" }}
                </Button>
                {move || (!stored.with(Vec::is_empty)).then(|| view! {
                    <ConfirmButton
                        label="Clear"
                        confirm_label="Clear credentials"
                        destructive=true
                        variant=ButtonVariant::Ghost
                        icon=Icon::Trash
                        busy=clearing
                        title="Remove every stored value; the integration stops authenticating"
                        on_confirm=clear
                    />
                })}
            </div>
            // Enter in a value field submits through this hidden button.
            <button type="submit" class="sr-only" tabindex="-1" aria-hidden="true"></button>
        </form>
    }
}

#[component]
fn OAuthSection(gw: Gateway) -> impl IntoView {
    let oauth = Memo::new(move |_| {
        gw.detail
            .with(|d| d.as_ref().and_then(|d| d.config.oauth.clone()))
    });
    let connected = Memo::new(move |_| oauth.with(|o| o.as_ref().is_some_and(|o| o.connected)));
    let id = Memo::new(move |_| {
        gw.detail
            .with(|d| d.as_ref().map(|d| d.summary.id.clone()).unwrap_or_default())
    });
    let connecting = Signal::derive(move || {
        gw.connecting
            .with(|c| c.as_deref() == Some(id.get().as_str()))
    });
    let clearing = RwSignal::new(false);
    let disconnect = Callback::new(move |()| {
        let id = id.get_untracked();
        let name = gw.name_untracked(&id);
        clearing.set(true);
        spawn_detached(async move {
            match api::clear_gateway_credentials(&id).await {
                Ok(detail) => {
                    gw.toast
                        .show(format!("Disconnected {name}"), ToastKind::Info);
                    gw.apply(detail);
                }
                Err(e) => gw.toast.show(e.message().to_owned(), ToastKind::Error),
            }
            clearing.try_set(false);
        });
    });
    let rows = move || {
        let now = gw.now.get();
        let Some(o) = oauth.get().filter(|o| o.connected) else {
            return view! {
                <p class="small dim">"Not connected. Connect opens the server's sign-in page and returns here when it is done."</p>
            }
            .into_any();
        };
        let expiry = o.access_expires_at.map(|at| {
            let at = at as f64;
            let expired = at <= now;
            let text = if expired {
                format!("expired {}", format_relative_at(at, now))
            } else {
                format_relative_at(at, now)
            };
            let class = if expired && !o.refreshable {
                "num text-warn"
            } else {
                "num"
            };
            view! { <dt>"Access token"</dt><dd class=class title=format_absolute(at)>{text}</dd> }
        });
        view! {
            <dl class="kv">
                {o.issuer.map(|issuer| view! { <dt>"Issuer"</dt><dd class="mono">{issuer}</dd> })}
                {o.scope.map(|scope| view! { <dt>"Scope"</dt><dd class="mono">{scope}</dd> })}
                {expiry}
                <dt>"Refresh token"</dt>
                <dd>{if o.refreshable { "Stored: Omni renews access on its own" } else { "None: reconnect when access expires" }}</dd>
                {o.connected_at.map(|at| view! {
                    <dt>"Connected"</dt>
                    <dd class="num" title=format_absolute(at as f64)>{logic::ago(at as f64, now)}</dd>
                })}
            </dl>
        }
        .into_any()
    };
    view! {
        <section class="inspector-section stack">
            <h3>"OAuth connection"</h3>
            <div class="cluster">
                {move || if connected.get() {
                    view! { <Status kind=StatusKind::Ok label="Connected"/> }.into_any()
                } else {
                    view! { <Status kind=StatusKind::Warn label="Not connected"/> }.into_any()
                }}
            </div>
            {rows}
            <div class="cluster">
                {move || {
                    view! {
                        <Button
                            icon=Icon::External
                            busy=connecting
                            on_click=Callback::new(move |_| gw.connect(id.get_untracked()))
                        >
                            {if connected.get() { "Reconnect" } else { "Connect" }}
                        </Button>
                    }
                }}
                {move || connected.get().then(|| view! {
                    <ConfirmButton
                        label="Disconnect"
                        confirm_label="Confirm disconnect"
                        variant=ButtonVariant::Ghost
                        busy=clearing
                        title="Forget the stored tokens; the integration stops authenticating"
                        on_confirm=disconnect
                    />
                })}
            </div>
        </section>
    }
}

#[component]
pub(super) fn DeleteSection(gw: Gateway) -> impl IntoView {
    let id = Memo::new(move |_| {
        gw.detail
            .with(|d| d.as_ref().map(|d| d.summary.id.clone()).unwrap_or_default())
    });
    let typed = RwSignal::new(String::new());
    let deleting = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let matches = Signal::derive(move || {
        let id = id.get();
        !id.is_empty() && typed.with(|t| t.trim() == id)
    });
    let delete = Callback::new(move |()| {
        let id = id.get_untracked();
        let name = gw.name_untracked(&id);
        deleting.set(true);
        error.set(None);
        spawn_detached(async move {
            match api::delete_gateway_integration(&id).await {
                Ok(()) => {
                    gw.toast.show(format!("Deleted {name}"), ToastKind::Info);
                    gw.forget(&id);
                }
                Err(e) => {
                    error.try_set(Some(e.message().to_owned()));
                    deleting.try_set(false);
                }
            }
        });
    });
    view! {
        <section class="inspector-section stack gw-danger">
            <h3>"Delete integration"</h3>
            <p class="small dim">
                "Removes the integration, its stored credentials and its tool settings. Clients lose its tools on their next list."
            </p>
            <div class="field">
                <label class="field-label" for="gw-delete-confirm">
                    "Type " <span class="mono">{move || id.get()}</span> " to confirm"
                </label>
                <div class="cluster token-row">
                    <input
                        id="gw-delete-confirm"
                        class="input mono"
                        type="text"
                        autocomplete="off"
                        autocapitalize="off"
                        spellcheck="false"
                        prop:value=move || typed.get()
                        on:input=move |ev| typed.set(event_target_value(&ev))
                    />
                    <ConfirmButton
                        label="Delete"
                        destructive=true
                        variant=ButtonVariant::Danger
                        icon=Icon::Trash
                        busy=deleting
                        disabled=Signal::derive(move || !matches.get())
                        disabled_reason=Signal::derive(move || format!("Type {} first", id.get()))
                        on_confirm=delete
                    />
                </div>
            </div>
            {move || error.get().map(|e| view! { <p class="field-error" role="alert">{e}</p> })}
        </section>
    }
}
