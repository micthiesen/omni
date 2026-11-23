//! Watch a Kick stream through Omni's relay at its top quality.
//!
//! The relay holds a cushion behind live, so playback starts after a short
//! buffering wait and then rides out Kick's stalls and link refreshes. Opening
//! the page starts the relay so it prebuffers; nothing plays until Play loads
//! the relay playlist into a native HLS `<video>`. Fullscreen hands the element
//! to the system player. A player error never reloads the same playlist on
//! its own: that waits for Play, or for a new relay playlist.

use std::time::Duration;

use leptos::html::Video;
use leptos::prelude::*;
use omni_api::common::encode_uri_component;
use omni_api::streamers::{StreamRelayState, StreamRelayStatus, StreamerView};
use omni_web_kit::api;
use omni_web_kit::chrome::use_page_label;
use omni_web_kit::components::{Button, ButtonLink, ButtonVariant, Icon, PageHead};
use omni_web_kit::hooks::use_visible_poll;
use omni_web_kit::use_live_data;
use wasm_bindgen::JsCast;

fn seconds(ms: i64) -> String {
    #[allow(clippy::cast_precision_loss)]
    let s = ms as f64 / 1000.0;
    format!("{s:.0} s")
}

/// The player's own buffer ahead of the playhead.
fn player_ahead(video: &web_sys::HtmlVideoElement) -> Option<f64> {
    let ranges = video.buffered();
    let count = ranges.length();
    if count == 0 {
        return None;
    }
    let end = ranges.end(count - 1).ok()?;
    Some((end - video.current_time()).max(0.0))
}

/// `webkitEnterFullscreen` (iPad and iPhone: the system player), else the
/// standard Fullscreen API.
fn enter_fullscreen(video: &web_sys::HtmlVideoElement) {
    let webkit = js_sys::Reflect::get(video, &"webkitEnterFullscreen".into())
        .ok()
        .and_then(|f| f.dyn_into::<js_sys::Function>().ok());
    if let Some(enter) = webkit {
        let _ = enter.call0(video);
        return;
    }
    let element: &web_sys::Element = video.as_ref();
    let _ = element.request_fullscreen();
}

fn diagnostics(status: &StreamRelayStatus, ahead: Option<f64>, player_stalls: u32) -> String {
    let mut parts = Vec::new();
    if let Some(resolution) = &status.resolution {
        let fps = status
            .frame_rate
            .map(|f| format!(" {f:.0} fps"))
            .unwrap_or_default();
        parts.push(format!("{resolution}{fps}"));
    }
    if let Some(bandwidth) = status.bandwidth {
        #[allow(clippy::cast_precision_loss)]
        let mbps = bandwidth as f64 / 1_000_000.0;
        parts.push(format!("{mbps:.1} Mbps"));
    }
    let player = ahead
        .map(|a| format!(" + player {a:.0} s"))
        .unwrap_or_default();
    parts.push(format!("cushion {}{player}", seconds(status.cushion_ms)));
    parts.push(format!(
        "stalls {} relay, {player_stalls} player",
        status.stalls
    ));
    parts.push(format!("refreshes {}", status.refreshes));
    if status.discontinuities > 0 {
        parts.push(format!("restarts {}", status.discontinuities));
    }
    parts.join(" · ")
}

#[component]
pub fn StreamerWatchPage(#[prop(into)] streamer_id: String) -> impl IntoView {
    let live = use_live_data();
    let status = RwSignal::new(None::<StreamRelayStatus>);
    let error = RwSignal::new(None::<String>);
    // The playlist the video element has loaded; `None` until Play.
    let loaded = RwSignal::new(None::<String>);
    // The playlist that last failed in the player.
    let failed = RwSignal::new(None::<String>);
    let wants_play = RwSignal::new(false);
    let player_stalls = RwSignal::new(0u32);
    let ahead = RwSignal::new(None::<f64>);
    let video: NodeRef<Video> = NodeRef::new();

    let id = streamer_id.clone();
    let display = Memo::new(move |_| {
        live.snapshot.with(|s| {
            s.as_ref()
                .and_then(|s| s.streamers.iter().find(|x| x.id() == id))
                .map(|s| match s {
                    StreamerView::Live(l) => l.display_name.clone(),
                    StreamerView::Offline(o) => o.display_name.clone(),
                })
        })
    });
    let start_id = streamer_id.clone();
    let start = move || {
        let id = start_id.clone();
        leptos::task::spawn_local(async move {
            match api::start_streamer_relay(&id).await {
                Ok(next) => status.set(Some(next)),
                Err(e) => error.set(Some(e.message().to_owned())),
            }
        });
    };
    // Prebuffer while the viewer decides; the relay drops it after a few
    // minutes without Play.
    start();
    let fetch_id = streamer_id.clone();
    use_visible_poll(
        Signal::derive(String::new),
        move || {
            let id = fetch_id.clone();
            async move { api::fetch_streamer_relay(&id).await }
        },
        move |next| {
            if let Some(v) = video.get_untracked() {
                ahead.set(player_ahead(&v));
            }
            status.set(Some(next));
            error.set(None);
        },
        move |e: api::ApiClientError| error.set(Some(e.message().to_owned())),
        Duration::from_secs(2),
    );
    Effect::new(move |_| {
        let name = display.get().unwrap_or_else(|| "Stream".to_owned());
        document().set_title(&format!("Watch {name} · Omni"));
    });
    use_page_label(move || Some("Watch".to_owned()));

    let playable = Memo::new(move |_| {
        status.with(|s| {
            s.as_ref()
                .filter(|s| s.state != StreamRelayState::Starting)
                .and_then(|s| s.playlist_url.clone())
        })
    });
    // Load the relay playlist once the viewer asked to play, and again when
    // the relay restarted under a new playlist URL.
    Effect::new(move |_| {
        let Some(url) = playable.get() else {
            return;
        };
        let current = |l: &Option<String>| l.as_deref() == Some(url.as_str());
        if !wants_play.get() || loaded.with(current) || failed.with(current) {
            return;
        }
        let Some(v) = video.get_untracked() else {
            return;
        };
        v.set_src(&url);
        let _ = v.play();
        loaded.set(Some(url));
    });

    let stopped = move || {
        status.with(|s| {
            s.as_ref()
                .is_some_and(|s| matches!(s.state, StreamRelayState::Stopped))
        })
    };
    let play = move |_| {
        wants_play.set(true);
        failed.set(None);
        if stopped() {
            start();
        }
        // Inside the tap, so iOS lets the later programmatic play through.
        if let Some(v) = video.get_untracked() {
            let _ = v.play();
        }
    };
    let fullscreen = move |_| {
        if let Some(v) = video.get_untracked() {
            enter_fullscreen(&v);
        }
    };

    let state_line = move || {
        if let Some(message) = error.get() {
            return format!("Can't reach the relay: {message}");
        }
        let Some(s) = status.get() else {
            return "Starting the relay…".to_owned();
        };
        match s.state {
            StreamRelayState::Starting => match s.startup_remaining_ms {
                Some(ms) if s.resolution.is_some() => {
                    format!("Buffering, about {} to go", seconds(ms))
                }
                _ => "Connecting to Kick…".to_owned(),
            },
            StreamRelayState::Stopped => {
                "The relay stopped. Press Play to start it again.".to_owned()
            }
            StreamRelayState::Playing if failed.get().is_some() => {
                "Playback stopped. Press Play to resume.".to_owned()
            }
            StreamRelayState::Playing if loaded.get().is_none() => "Ready. Press Play.".to_owned(),
            StreamRelayState::Playing => s.message.unwrap_or_default(),
            StreamRelayState::Rebuffering => {
                format!(
                    "Kick stalled; refilling the cushion ({})",
                    seconds(s.cushion_ms)
                )
            }
            StreamRelayState::Offline => s
                .message
                .unwrap_or_else(|| "The channel is offline on Kick".to_owned()),
            StreamRelayState::Error => s
                .message
                .map(|m| format!("Retrying Kick: {m}"))
                .unwrap_or_else(|| "Retrying Kick…".to_owned()),
        }
    };
    let offline =
        move || status.with(|s| s.as_ref().map(|s| s.state) == Some(StreamRelayState::Offline));
    let details_href = format!("/streamers/{}", encode_uri_component(&streamer_id));

    view! {
        <PageHead
            title=Signal::derive(move || display.get().map(|n| format!("Watch {n}")).unwrap_or_else(|| "Watch".to_owned()))
            lede="Kick at its top quality through Omni's relay, about 50 s behind live."
            sentence=true
            actions=ViewFn::from(move || view! {
                <ButtonLink to=details_href.clone()>"Details"</ButtonLink>
            })
        />
        <div class="watch-stage" class:offline=offline>
            <video
                node_ref=video
                class="watch-video"
                controls=true
                playsinline=true
                preload="none"
                on:waiting=move |_| {
                    if loaded.get_untracked().is_some() {
                        player_stalls.update(|n| *n += 1);
                    }
                }
                on:error=move |_| {
                    failed.set(loaded.get_untracked());
                    loaded.set(None);
                    wants_play.set(false);
                }
            ></video>
            <Show when=move || loaded.get().is_none()>
                <div class="watch-overlay" aria-live="polite">
                    <p class="watch-state">{state_line}</p>
                </div>
            </Show>
        </div>
        <div class="watch-bar">
            <div class="cluster">
                <Button
                    variant=ButtonVariant::Primary
                    icon=Icon::Play
                    disabled=Signal::derive(move || playable.get().is_none() && !stopped())
                    on_click=Callback::new(play)
                >"Play"</Button>
                <Button
                    disabled=Signal::derive(move || loaded.get().is_none())
                    on_click=Callback::new(fullscreen)
                >"Fullscreen"</Button>
            </div>
            <p class="watch-diag mono">
                {move || {
                    let line = loaded.get().is_some().then(state_line).filter(|l| !l.is_empty());
                    let numbers = status.get().map(|s| diagnostics(&s, ahead.get(), player_stalls.get()));
                    [line, numbers].into_iter().flatten().collect::<Vec<_>>().join(" · ")
                }}
            </p>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostics_name_quality_buffer_and_counters() {
        let status = StreamRelayStatus {
            state: StreamRelayState::Playing,
            playlist_url: Some("/p".into()),
            cushion_ms: 29_600,
            target_cushion_ms: 30_000,
            startup_remaining_ms: None,
            resolution: Some("1920x1080".into()),
            frame_rate: Some(60.0),
            bandwidth: Some(8_992_204),
            stalls: 0,
            refreshes: 3,
            discontinuities: 0,
            buffered_bytes: 1,
            message: None,
        };
        assert_eq!(
            diagnostics(&status, Some(17.2), 1),
            "1920x1080 60 fps · 9.0 Mbps · cushion 30 s + player 17 s · stalls 0 relay, 1 player · refreshes 3"
        );
    }
}
