//! The hero: what the endpoint serves (readout band) and how to connect a
//! harness to it (URLs and setup snippets).

use leptos::prelude::*;
use omni_web_kit::components::{Panel, Readout, ReadoutBand, SegOption, Segmented, Tone};

use super::logic::{self, Counts, TOKEN_PLACEHOLDER};
use super::{CopyButton, Gateway};

/// The page origin (`http://omni.boris`).
pub(super) fn page_origin() -> String {
    window().location().origin().unwrap_or_default()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Harness {
    ClaudeCode,
    Codex,
    ChatGpt,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Reach {
    Lan,
    Public,
}

#[component]
pub(super) fn EndpointOverview(gw: Gateway) -> impl IntoView {
    let counts = Memo::new(move |_| {
        gw.overview
            .with(|o| o.as_ref().map(Counts::of).unwrap_or_default())
    });
    let pick = move |f: fn(&Counts) -> String| Signal::derive(move || counts.with(f));
    view! {
        <ReadoutBand cols=4 aria_label="Gateway">
            <Readout label="Tools served" value=pick(|c| c.exposed.to_string())>
                {move || counts.with(|c| format!(
                    "{} Omni · {} from integrations",
                    c.native,
                    c.exposed.saturating_sub(c.native)
                ))}
            </Readout>
            <Readout
                label="Integrations"
                value=pick(|c| c.integrations.to_string())
                tone=Signal::derive(move || if counts.with(|c| c.attention) > 0 { Tone::Warn } else { Tone::Neutral })
            >
                {move || counts.with(|c| {
                    if c.attention > 0 {
                        format!("{} ready · {} need attention", c.ready, c.attention)
                    } else {
                        format!("{} ready", c.ready)
                    }
                })}
            </Readout>
            <Readout label="Clients" value=pick(|c| (c.tokens + c.grants).to_string())>
                {move || counts.with(|c| {
                    let mut line = format!(
                        "{} · {}",
                        logic::plural(c.tokens, "token", "tokens"),
                        logic::plural(c.grants, "OAuth grant", "OAuth grants")
                    );
                    if c.pending > 0 {
                        line.push_str(&format!(" · {} waiting", c.pending));
                    }
                    line
                })}
            </Readout>
            <Readout
                label="Event feeds"
                value=pick(|c| c.feeds.to_string())
                tone=Signal::derive(move || if counts.with(|c| c.gaps) > 0 { Tone::Warn } else { Tone::Neutral })
            >
                {move || counts.with(|c| {
                    if c.gaps > 0 {
                        format!("{} active · {} gaps", c.active_feeds, c.gaps)
                    } else {
                        format!("{} active", c.active_feeds)
                    }
                })}
            </Readout>
        </ReadoutBand>
        <EndpointPanel gw/>
    }
}

#[component]
fn EndpointPanel(gw: Gateway) -> impl IntoView {
    let origin = page_origin();
    let lan = Memo::new(move |_| {
        gw.overview.with(|o| {
            o.as_ref()
                .map(|o| logic::lan_url(&origin, &o.path))
                .unwrap_or_default()
        })
    });
    let public = Memo::new(move |_| {
        gw.overview
            .with(|o| o.as_ref().and_then(|o| o.public_url.clone()))
    });
    let oauth = Memo::new(move |_| {
        gw.overview
            .with(|o| o.as_ref().is_some_and(|o| o.oauth_available))
    });
    let harness = RwSignal::new(Harness::ClaudeCode);
    let reach = RwSignal::new(Reach::Public);
    // ChatGPT disappears when OAuth is unavailable; fall back to Claude Code.
    Effect::new(move |_| {
        if !oauth.get() && harness.get_untracked() == Harness::ChatGpt {
            harness.set(Harness::ClaudeCode);
        }
    });
    let url = Memo::new(move |_| match (reach.get(), public.get()) {
        (Reach::Public, Some(public)) => public,
        _ => lan.get(),
    });
    let harness_options = Signal::derive(move || {
        let mut options = vec![
            SegOption::new(Harness::ClaudeCode, "Claude Code"),
            SegOption::new(Harness::Codex, "Codex"),
        ];
        if oauth.get() {
            options.push(SegOption::new(Harness::ChatGpt, "ChatGPT"));
        }
        options
    });
    let reach_options = Signal::derive(|| {
        vec![
            SegOption::new(Reach::Public, "Public URL"),
            SegOption::new(Reach::Lan, "LAN URL"),
        ]
    });
    let snippet = Memo::new(move |_| match harness.get() {
        Harness::ClaudeCode => logic::claude_code_command(&url.get(), TOKEN_PLACEHOLDER),
        Harness::Codex => logic::codex_config(&url.get()),
        Harness::ChatGpt => public.get().unwrap_or_default(),
    });

    view! {
        <Panel title="Endpoint" class="gw-endpoint">
            <div class="rows">
                <div class="row gw-url">
                    <span class="gw-url-k">"LAN"</span>
                    <span class="mono gw-url-v" title=move || lan.get()>{move || lan.get()}</span>
                    <CopyButton text=lan label="Copy the LAN URL"/>
                </div>
                <div class="row gw-url">
                    <span class="gw-url-k">"Public"</span>
                    {move || match public.get() {
                        Some(public_url) => {
                            let text = public_url.clone();
                            let title = public_url.clone();
                            view! {
                                <span class="mono gw-url-v" title=title>{public_url}</span>
                                <CopyButton text=Signal::derive(move || text.clone()) label="Copy the public URL"/>
                            }
                            .into_any()
                        }
                        None => view! {
                            <span class="small muted gw-url-v">
                                "Not configured. Set " <code class="mono">"OMNI_GATEWAY_PUBLIC_URL"</code>
                                " to reach the gateway from outside the LAN and to connect ChatGPT."
                            </span>
                        }
                        .into_any(),
                    }}
                </div>
            </div>
            <div class="panel-body gw-setup">
                <div class="toolbar">
                    <Segmented
                        options=harness_options
                        value=harness
                        on_change=Callback::new(move |h| harness.set(h))
                        aria_label="Set up a client"
                        small=true
                    />
                    {move || (public.with(Option::is_some) && harness.get() != Harness::ChatGpt).then(|| view! {
                        <Segmented
                            options=reach_options
                            value=reach
                            on_change=Callback::new(move |r| reach.set(r))
                            aria_label="Which URL the snippet uses"
                            small=true
                        />
                    })}
                </div>
                <div class="gw-snippet">
                    <pre class="code-block" aria-label="Setup snippet">{move || snippet.get()}</pre>
                    <CopyButton text=snippet label="Copy the snippet" variant=omni_web_kit::components::ButtonVariant::Secondary/>
                </div>
                <p class="field-help">
                    {move || match harness.get() {
                        Harness::ClaudeCode => view! {
                            "Run this where Claude Code runs. Create a client token under Client tokens and put it in place of "
                            <code class="mono">{TOKEN_PLACEHOLDER}</code>
                            "; the token's reveal can copy the finished command."
                        }
                        .into_any(),
                        Harness::Codex => view! {
                            "Add this to " <code class="mono">"~/.codex/config.toml"</code> " and set "
                            <code class="mono">{logic::CODEX_TOKEN_ENV}</code>
                            " to a client token in the environment Codex runs in."
                        }
                        .into_any(),
                        Harness::ChatGpt => view! {
                            "Add a custom connector in ChatGPT with this URL and OAuth sign-in. Omni then shows a code; approve the matching request under Waiting for approval on this page."
                        }
                        .into_any(),
                    }}
                </p>
            </div>
        </Panel>
    }
}
