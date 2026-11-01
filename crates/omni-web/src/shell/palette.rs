//! The command palette: every destination, live streamers and tasks from
//! data the SPA already loads. It only navigates;
//! choosing a task opens `/operations#inspect=<Task>`.

use leptos::html::Input;
use leptos::prelude::*;
use omni_api::common::encode_uri_component;
use omni_api::streamers::StreamerView;
use omni_web_kit::components::streamers::streamer_path;
use omni_web_kit::components::{Glyph, Icon, IconSize};
use omni_web_kit::hooks::{use_is_phone, use_modal};
use omni_web_kit::live::use_live_data;
use omni_web_kit::router::navigate;
use omni_web_kit::utils::format::{format_compact_number, task_label};

use super::nav::{Group, LIVE_HREF, NAV, STREAMER_CONFIG_HREF};

/// One palette row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub group: &'static str,
    pub label: String,
    pub hint: Option<String>,
    pub href: String,
    pub icon: Icon,
}

pub const GROUP_ORDER: [&str; 4] = ["Live now", "Pages", "Streamers", "Tasks"];
const PER_GROUP: usize = 8;

/// Case-insensitive match of every query word against label plus hint.
pub fn matches(entry: &Entry, query: &str) -> bool {
    let haystack = format!(
        "{} {} {}",
        entry.label,
        entry.hint.as_deref().unwrap_or(""),
        entry.group
    )
    .to_lowercase();
    query
        .to_lowercase()
        .split_whitespace()
        .all(|word| haystack.contains(word))
}

/// Filters and orders entries by group. With no query only pages show; live
/// channels, streamers and tasks need a query.
pub fn filter_entries(entries: &[Entry], query: &str) -> Vec<Entry> {
    let query = query.trim();
    GROUP_ORDER
        .iter()
        .flat_map(|group| {
            entries
                .iter()
                .filter(move |e| e.group == *group)
                .filter(|e| {
                    if query.is_empty() {
                        e.group == "Pages"
                    } else {
                        matches(e, query)
                    }
                })
                .take(if query.is_empty() {
                    usize::MAX
                } else {
                    PER_GROUP
                })
                .cloned()
                .collect::<Vec<_>>()
        })
        .collect()
}

fn page_entries() -> Vec<Entry> {
    let mut pages: Vec<Entry> = NAV
        .iter()
        .map(|item| Entry {
            group: "Pages",
            label: item.label.to_owned(),
            hint: (item.group != Group::Top).then(|| item.group.label().to_owned()),
            href: item.href.to_owned(),
            icon: item.icon,
        })
        .collect();
    pages.insert(
        1,
        Entry {
            group: "Pages",
            label: "Live".to_owned(),
            hint: Some("On air".to_owned()),
            href: LIVE_HREF.to_owned(),
            icon: Icon::Live,
        },
    );
    pages.insert(
        2,
        Entry {
            group: "Pages",
            label: "Manage streamers".to_owned(),
            hint: Some("Live".to_owned()),
            href: STREAMER_CONFIG_HREF.to_owned(),
            icon: Icon::Pencil,
        },
    );
    pages
}

#[component]
pub fn Palette(on_close: Callback<()>) -> impl IntoView {
    let live = use_live_data();
    let query = RwSignal::new(String::new());
    let selected = RwSignal::new(0usize);
    let input = NodeRef::<Input>::new();
    let panel = use_modal(move || on_close.run(()));
    let phone = use_is_phone();
    // On a phone, focusing the input would raise the keyboard over the list;
    // `use_modal` focuses the close button there instead.
    Effect::new(move |_| {
        if let Some(input) = input.get()
            && !phone.get_untracked()
        {
            let _ = input.focus();
        }
    });

    let entries = Memo::new(move |_| {
        let mut all = Vec::new();
        live.snapshot.with(|s| {
            let Some(s) = s else { return };
            for streamer in &s.streamers {
                match streamer {
                    StreamerView::Live(l) => all.push(Entry {
                        group: "Live now",
                        label: l.display_name.clone(),
                        hint: Some(format!(
                            "{} watching",
                            format_compact_number(
                                l.viewer_count.unwrap_or(l.max_viewer_count) as f64
                            )
                        )),
                        href: streamer_path(&l.id),
                        icon: Icon::Live,
                    }),
                    StreamerView::Offline(o) => all.push(Entry {
                        group: "Streamers",
                        label: o.display_name.clone(),
                        hint: Some("offline".to_owned()),
                        href: streamer_path(&o.id),
                        icon: Icon::Live,
                    }),
                }
            }
            for task in &s.tasks {
                all.push(Entry {
                    group: "Tasks",
                    label: task_label(&task.name, task.display_name.as_deref()),
                    hint: Some(task.name.clone()),
                    href: format!("/operations#inspect={}", encode_uri_component(&task.name)),
                    icon: Icon::Pulse,
                });
            }
        });
        all.extend(page_entries());
        all
    });
    let visible = Memo::new(move |_| query.with(|q| filter_entries(&entries.get(), q)));
    Effect::new(move |_| {
        query.track();
        selected.set(0);
    });

    let choose = move |index: usize| {
        if let Some(entry) = visible.with_untracked(|v| v.get(index).cloned()) {
            on_close.run(());
            navigate(&entry.href);
        }
    };
    let on_key = move |event: web_sys::KeyboardEvent| {
        let count = visible.with_untracked(Vec::len);
        match event.key().as_str() {
            "ArrowDown" => {
                event.prevent_default();
                if count > 0 {
                    selected.update(|s| *s = (*s + 1) % count);
                }
            }
            "ArrowUp" => {
                event.prevent_default();
                if count > 0 {
                    selected.update(|s| *s = (*s + count - 1) % count);
                }
            }
            "Enter" => {
                event.prevent_default();
                choose(selected.get_untracked());
            }
            _ => {}
        }
    };
    // Keep the selection in view.
    Effect::new(move |_| {
        let index = selected.get();
        if let Some(el) = document().get_element_by_id(&format!("palette-opt-{index}")) {
            let options = web_sys::ScrollIntoViewOptions::new();
            options.set_block(web_sys::ScrollLogicalPosition::Nearest);
            el.scroll_into_view_with_scroll_into_view_options(&options);
        }
    });

    let list = move || {
        let items = visible.get();
        if items.is_empty() {
            return view! {
                <p class="palette-empty">{format!("No matches for \u{201c}{}\u{201d}", query.get())}</p>
            }
            .into_any();
        }
        let mut last_group = "";
        items
            .into_iter()
            .enumerate()
            .map(|(index, entry)| {
                let heading = (entry.group != last_group).then(|| {
                    view! { <div class="palette-group" role="presentation">{entry.group}</div> }
                });
                last_group = entry.group;
                let href = entry.href.clone();
                view! {
                    {heading}
                    <div
                        id=format!("palette-opt-{index}")
                        class="palette-item"
                        role="option"
                        aria-selected=move || (selected.get() == index).to_string()
                        on:mousemove=move |_| {
                            if selected.get_untracked() != index {
                                selected.set(index);
                            }
                        }
                        on:click=move |_| choose(index)
                        data-href=href
                    >
                        <Glyph icon=entry.icon size=IconSize::Small/>
                        <span class="truncate">{entry.label}</span>
                        {entry.hint.map(|h| view! { <span class="hint truncate">{h}</span> })}
                    </div>
                }
            })
            .collect_view()
            .into_any()
    };

    view! {
        <div class="palette-scrim" on:click=move |ev: web_sys::MouseEvent| {
            if ev.target() == ev.current_target() {
                on_close.run(());
            }
        }>
            <div
                node_ref=panel
                id="nav-palette"
                class="palette"
                role="dialog"
                aria-modal="true"
                aria-label="Go to"
                tabindex="-1"
            >
                <div class="palette-input-row">
                    <Glyph icon=Icon::Search/>
                    <input
                        node_ref=input
                        class="palette-input"
                        type="text"
                        role="combobox"
                        aria-expanded="true"
                        aria-controls="palette-list"
                        aria-activedescendant=move || format!("palette-opt-{}", selected.get())
                        aria-autocomplete="list"
                        placeholder="Go to a page, streamer, task or subject"
                        autocomplete="off"
                        spellcheck="false"
                        prop:value=move || query.get()
                        on:input=move |ev| query.set(event_target_value(&ev))
                        on:keydown=on_key
                    />
                    <button
                        type="button"
                        class="btn ghost icon-only only-phone palette-close"
                        aria-label="Close"
                        on:click=move |_| on_close.run(())
                    >
                        <Glyph icon=Icon::Close/>
                    </button>
                </div>
                <div id="palette-list" class="palette-list" role="listbox" aria-label="Results">
                    {list}
                </div>
                <div class="palette-foot" aria-hidden="true">
                    <span><kbd class="kbd">"↑"</kbd> <kbd class="kbd">"↓"</kbd> " move"</span>
                    <span><kbd class="kbd">"↵"</kbd> " open"</span>
                    <span><kbd class="kbd">"esc"</kbd> " close"</span>
                </div>
            </div>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(group: &'static str, label: &str) -> Entry {
        Entry {
            group,
            label: label.into(),
            hint: None,
            href: "/".into(),
            icon: Icon::Home,
        }
    }

    #[test]
    fn empty_query_shows_only_pages_and_search_still_finds_live_channels() {
        let entries = [
            entry("Tasks", "Live check"),
            entry("Pages", "Home"),
            entry("Live now", "Hutch"),
            entry("Streamers", "Destiny"),
        ];
        let shown: Vec<_> = filter_entries(&entries, "")
            .into_iter()
            .map(|e| e.label)
            .collect();
        assert_eq!(shown, ["Home"]);
        let shown: Vec<_> = filter_entries(&entries, "hutch")
            .into_iter()
            .map(|e| e.label)
            .collect();
        assert_eq!(shown, ["Hutch"]);
        let shown: Vec<_> = filter_entries(&entries, "live CHE")
            .into_iter()
            .map(|e| e.label)
            .collect();
        assert_eq!(shown, ["Live check"]);
    }
}
