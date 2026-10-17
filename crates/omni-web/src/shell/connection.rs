//! The connection readout (rail footer and phone top bar), mapped 1:1 onto
//! `ConnectionState`, plus the "Update" state once a snapshot reports a newer
//! build. The mark is the Omni orbit: it turns slowly while live and its dot
//! pops once per snapshot, holds still while polling, spins a short arc while
//! reconnecting and breaks apart offline. Activating it always reloads.

use leptos::html::Span;
use leptos::prelude::*;
use omni_api::build::BuildChange;
use omni_web_kit::hooks::use_now;
use omni_web_kit::live::{ConnectionState, use_live_data};

/// `(class, text, detail)` for a state.
pub fn connection_copy(
    state: ConnectionState,
    error: bool,
    seconds: Option<u64>,
) -> (&'static str, String, Option<String>) {
    match state {
        ConnectionState::Live => (
            "",
            "Live".to_owned(),
            Some(match seconds {
                Some(s) if s < 60 => format!("{s}s"),
                Some(s) => format!("{}m", s / 60),
                None => "now".to_owned(),
            }),
        ),
        ConnectionState::Polling if error => {
            ("fault", "Offline".to_owned(), Some("retry".to_owned()))
        }
        ConnectionState::Polling => ("warn", "Polling".to_owned(), Some("every 10 s".to_owned())),
        ConnectionState::Connecting if error => {
            ("fault", "Offline".to_owned(), Some("retry".to_owned()))
        }
        ConnectionState::Connecting => ("warn", "Reconnecting…".to_owned(), None),
    }
}

/// The button's tooltip: always a reload, saying what it picks up.
pub fn connection_title(change: Option<BuildChange>) -> String {
    match change {
        Some(change) => format!(
            "Update available ({} changed). Reload to update",
            change.describe()
        ),
        None => "Reload".to_owned(),
    }
}

/// `compact` renders only the dot (phone top bar), and an "Update" tag while an
/// update is available. Clicking or tapping always reloads the page.
#[component]
pub fn Connection(#[prop(optional)] compact: bool) -> impl IntoView {
    let live = use_live_data();
    let now = use_now(1000);
    let dot = NodeRef::<Span>::new();
    let copy = Memo::new(move |_| {
        let updated = live.updated_at.get();
        let seconds =
            (updated > 0.0).then(|| ((now.get() - updated) / 1000.0).max(0.0).floor() as u64);
        connection_copy(
            live.connection.get(),
            live.error.with(Option::is_some),
            seconds,
        )
    });
    // Pop the dot on every snapshot while live.
    Effect::new(move |previous: Option<f64>| {
        let updated = live.updated_at.get();
        if previous.is_some_and(|p| p != updated)
            && live.connection.get_untracked() == ConnectionState::Live
            && let Some(el) = dot.get_untracked()
        {
            let list = el.class_list();
            let _ = list.remove_1("pop");
            let _ = el.offset_width();
            let _ = list.add_1("pop");
        }
        updated
    });
    let mark_class = move || {
        let (tone, _, _) = copy.get();
        let motion = match (live.connection.get(), tone) {
            (_, "fault") => "",
            (ConnectionState::Live, _) => " orbiting",
            (ConnectionState::Connecting, _) => " seeking",
            (ConnectionState::Polling, _) => "",
        };
        format!("conn-mark {tone}{motion}")
    };
    let update = move || live.build_change.get().is_some();
    let label = move || {
        let (_, text, detail) = copy.get();
        let state = match detail {
            Some(d) => format!("{text} · {d}"),
            None => text,
        };
        if update() {
            format!("{state}. Update available, reload")
        } else {
            format!("{state}. Reload")
        }
    };
    view! {
        <button
            type="button"
            class=move || {
                format!("conn {}{}", copy.get().0, if update() { " has-update" } else { "" })
            }
            title=move || connection_title(live.build_change.get())
            aria-label=move || format!("Connection: {}", label())
            on:click=move |_| {
                let _ = window().location().reload();
            }
        >
            <span node_ref=dot class=mark_class aria-hidden="true">
                <ConnectionMark/>
            </span>
            {(!compact).then(|| view! { <span role="status">{move || copy.get().1}</span> })}
            <Show when=update>
                <span class="conn-update">"Update"</span>
            </Show>
            {(!compact).then(|| view! { <span class="detail num">{move || copy.get().2}</span> })}
        </button>
    }
}

/// The orbit mark at 16 px: a faint track, a trail that brightens toward its
/// head, the leading dot and the halo that breathes while an update waits.
/// Geometry matches `scripts/brand/icons.py` (dot at the upper right, the ring
/// ending 35 degrees either side of it).
#[component]
fn ConnectionMark() -> impl IntoView {
    view! {
        <svg class="conn-orbit" viewBox="0 0 16 16" width="16" height="16">
            <circle class="track" cx="8" cy="8" r="5.5"/>
            <g class="spin">
                <circle
                    class="trail"
                    cx="8"
                    cy="8"
                    r="5.5"
                    pathLength="360"
                    stroke-dasharray="290 70"
                    transform="rotate(-10 8 8)"
                />
                <circle
                    class="head"
                    cx="8"
                    cy="8"
                    r="5.5"
                    pathLength="360"
                    stroke-dasharray="110 250"
                    transform="rotate(170 8 8)"
                />
                <circle class="halo" cx="11.89" cy="4.11" r="3.2"/>
                <circle class="dot" cx="11.89" cy="4.11" r="1.9"/>
            </g>
        </svg>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copy_maps_each_state() {
        assert_eq!(
            connection_copy(ConnectionState::Live, false, Some(2)),
            ("", "Live".into(), Some("2s".into()))
        );
        assert_eq!(
            connection_copy(ConnectionState::Polling, false, None).0,
            "warn"
        );
        assert_eq!(
            connection_copy(ConnectionState::Polling, true, None).1,
            "Offline"
        );
        assert_eq!(
            connection_copy(ConnectionState::Connecting, false, None).1,
            "Reconnecting…"
        );
    }

    #[test]
    fn title_always_offers_a_reload() {
        assert_eq!(connection_title(None), "Reload");
        assert_eq!(
            connection_title(Some(BuildChange::Both)),
            "Update available (app and server changed). Reload to update"
        );
    }
}
