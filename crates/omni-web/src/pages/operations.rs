//! Operations: task health readouts, the task table grouped by cadence,
//! the task inspector (`#inspect=<Task>`) with run history and live logs,
//! and the run log.

use std::collections::HashMap;

use futures::stream::{self, StreamExt as _};
use leptos::prelude::*;
use omni_api::runs::{Run, RunStatus};
use omni_api::tasks::TaskInfo;
use omni_web_kit::api;
use omni_web_kit::components::badges::{run_status_kind, run_status_label};
use omni_web_kit::components::task_display::{detail_class, run_detail, run_duration};
use omni_web_kit::components::{
    Button, ButtonSize, ButtonVariant, CellKind, EmptyState, ErrorState, Icon, Inspector,
    LogViewer, LogWell, PageHead, Panel, Readout, ReadoutBand, RunButton, RunCell, RunLog,
    RunStrip, SearchField, SegOption, Segmented, SkeletonRows, Status, StatusKind, Tag, TimeLane,
    Tone, TriggerBadge,
};
use omni_web_kit::hooks::{
    hash_param, query_param, replace_hash_param, replace_query_param, use_is_wide, use_now,
};
use omni_web_kit::live::use_live_data;
use omni_web_kit::task::{on_cleanup_local, spawn_scoped};
use omni_web_kit::utils::cron::describe_cron;
use omni_web_kit::utils::format::{format_absolute, format_relative_at, task_label};
use omni_web_kit::utils::js::parse_date_ms;
use omni_web_kit::utils::tasks::{
    Cadence, TaskHealth, cadence, format_next, next_run_ms, task_health,
};
use wasm_bindgen::JsCast as _;
use wasm_bindgen::closure::Closure;

const STRIP: usize = 12;
/// The server caps `limit` at 200.
const HISTORY_FETCH: u32 = 200;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskFilter {
    All,
    Attention,
    Running,
}

impl TaskFilter {
    fn param(self) -> Option<&'static str> {
        match self {
            TaskFilter::All => None,
            TaskFilter::Attention => Some("attention"),
            TaskFilter::Running => Some("running"),
        }
    }

    fn from_param(value: Option<&str>) -> Self {
        match value {
            Some("attention") => TaskFilter::Attention,
            Some("running") => TaskFilter::Running,
            _ => TaskFilter::All,
        }
    }
}

/// The word next to a task's status shape.
fn health_word(health: TaskHealth) -> &'static str {
    match health {
        TaskHealth::Running => "Running",
        TaskHealth::Fault => "Failing",
        TaskHealth::Degraded => "Degraded",
        TaskHealth::Stale => "Stale",
        TaskHealth::Ok => "Healthy",
        TaskHealth::Idle => "No runs yet",
    }
}

fn health_kind(health: TaskHealth) -> StatusKind {
    match health {
        TaskHealth::Running => StatusKind::Running,
        TaskHealth::Fault => StatusKind::Fault,
        TaskHealth::Degraded => StatusKind::Warn,
        TaskHealth::Stale => StatusKind::Stale,
        TaskHealth::Ok => StatusKind::Ok,
        TaskHealth::Idle => StatusKind::Idle,
    }
}

pub fn matches_filter(task: &TaskInfo, filter: TaskFilter, query: &str, now: f64) -> bool {
    let needle = query.trim().to_lowercase();
    let haystack = format!(
        "{} {}",
        task_label(&task.name, task.display_name.as_deref()),
        task.name
    )
    .to_lowercase();
    haystack.contains(&needle)
        && match filter {
            TaskFilter::All => true,
            TaskFilter::Running => task.running,
            TaskFilter::Attention => task_health(task, now).needs_attention(),
        }
}

/// Visible task names grouped by cadence, each group ordered by next run.
pub fn grouped_tasks(
    tasks: &[TaskInfo],
    filter: TaskFilter,
    query: &str,
    now: f64,
) -> Vec<(Cadence, Vec<String>)> {
    [Cadence::Realtime, Cadence::Frequent, Cadence::Scheduled]
        .into_iter()
        .filter_map(|group| {
            let mut list: Vec<&TaskInfo> = tasks
                .iter()
                .filter(|t| cadence(t) == group && matches_filter(t, filter, query, now))
                .collect();
            list.sort_by(|a, b| {
                next_run_ms(a)
                    .unwrap_or(f64::MAX)
                    .total_cmp(&next_run_ms(b).unwrap_or(f64::MAX))
            });
            (!list.is_empty()).then(|| (group, list.into_iter().map(|t| t.name.clone()).collect()))
        })
        .collect()
}

fn cell_kind(run: &Run) -> CellKind {
    match run.status {
        RunStatus::Running => CellKind::Running,
        RunStatus::Error => CellKind::Fault,
        RunStatus::Degraded => CellKind::Warn,
        RunStatus::Success
            if run
                .summary
                .as_deref()
                .is_some_and(|s| s.starts_with("skipped:")) =>
        {
            CellKind::Warn
        }
        RunStatus::Success => CellKind::Ok,
    }
}

/// Newest-first runs per task, merging fetched history with the snapshot.
fn merge_history(fetched: &[Run], snapshot: &[Run]) -> HashMap<String, Vec<Run>> {
    let mut map: HashMap<String, Vec<Run>> = HashMap::new();
    for run in snapshot.iter().chain(fetched) {
        let list = map.entry(run.task_name.clone()).or_default();
        if !list.iter().any(|r| r.run_id == run.run_id) {
            list.push(run.clone());
        }
    }
    for list in map.values_mut() {
        list.sort_by_key(|r| std::cmp::Reverse(r.started_at));
    }
    map
}

#[component]
pub fn OperationsPage() -> impl IntoView {
    let live = use_live_data();
    let has_snapshot = Memo::new(move |_| live.snapshot.with(Option::is_some));
    move || {
        if has_snapshot.get() {
            return view! { <OperationsContent/> }.into_any();
        }
        match live.error.get() {
            Some(e) => view! {
                <ErrorState title="Operations could not load" raw=e page=true retry=Callback::new(|()| { let _ = window().location().reload(); })/>
            }
            .into_any(),
            None => view! { <SkeletonRows count=8/> }.into_any(),
        }
    }
}

#[component]
fn OperationsContent() -> impl IntoView {
    let live = use_live_data();
    let now = use_now(1000);
    let wide = use_is_wide();
    let query = RwSignal::new(String::new());
    let filter = RwSignal::new(TaskFilter::from_param(query_param("filter").as_deref()));
    let inspect = RwSignal::new(hash_param("inspect"));
    let log_run = RwSignal::new(None::<Run>);
    let fetched = RwSignal::new(Vec::<Run>::new());
    let history_loaded = RwSignal::new(false);

    // Re-read `#inspect=` on palette navigation and history moves.
    let on_pop = Closure::<dyn FnMut()>::new(move || inspect.set(hash_param("inspect")));
    let win = window();
    let _ = win.add_event_listener_with_callback("popstate", on_pop.as_ref().unchecked_ref());
    on_cleanup_local(move || {
        let _ =
            win.remove_event_listener_with_callback("popstate", on_pop.as_ref().unchecked_ref());
    });

    // Deep history for strips; refetched when a non-realtime run finishes.
    let scheduled_finish = Memo::new(move |_| {
        live.snapshot.with(|s| {
            s.as_ref().and_then(|s| {
                s.tasks
                    .iter()
                    .filter(|t| cadence(t) != Cadence::Realtime)
                    .filter_map(|t| t.last_run.as_ref()?.finished_at)
                    .max()
            })
        })
    });
    // The global fetch is capped server-side and realtime runs fill it, so
    // tasks left with a short strip backfill from their own history.
    Effect::new(move |_| {
        scheduled_finish.track();
        let names: Vec<String> = live.snapshot.with_untracked(|s| {
            s.as_ref()
                .map(|s| s.tasks.iter().map(|t| t.name.clone()).collect())
                .unwrap_or_default()
        });
        spawn_scoped(async move {
            let Ok(res) = api::fetch_task_runs(None, Some(HISTORY_FETCH)).await else {
                return;
            };
            let mut runs = res.runs;
            let short: Vec<String> = names
                .into_iter()
                .filter(|name| runs.iter().filter(|r| &r.task_name == name).count() < STRIP)
                .collect();
            fetched.set(runs.clone());
            history_loaded.set(true);
            if short.is_empty() {
                return;
            }
            let extra: Vec<_> =
                stream::iter(short)
                    .map(|name| async move {
                        api::fetch_task_runs(Some(&name), Some(STRIP as u32)).await
                    })
                    .buffer_unordered(3)
                    .collect()
                    .await;
            for res in extra.into_iter().flatten() {
                runs.extend(res.runs);
            }
            fetched.set(runs);
        });
    });
    let history = Memo::new(move |_| {
        fetched.with(|f| {
            live.snapshot
                .with(|s| merge_history(f, s.as_ref().map_or(&[][..], |s| &s.runs)))
        })
    });

    let tasks = Signal::derive(move || {
        live.snapshot
            .with(|s| s.as_ref().map(|s| s.tasks.clone()).unwrap_or_default())
    });
    let minute = use_now(30_000);
    let groups = Memo::new(move |_| {
        tasks.with(|t| grouped_tasks(t, filter.get(), &query.get(), minute.get()))
    });
    // The table closure reads only emptiness; groups and rows are keyed
    // `For`s, so reordering after a run moves rows instead of rebuilding them.
    let no_groups = Memo::new(move |_| groups.with(Vec::is_empty));
    let counts = Memo::new(move |_| {
        let now = minute.get();
        tasks.with(|t| {
            let attention = t
                .iter()
                .filter(|t| task_health(t, now).needs_attention())
                .count();
            let running = t.iter().filter(|t| t.running).count();
            let failing = t
                .iter()
                .filter(|t| task_health(t, now) == TaskHealth::Fault)
                .count();
            let stale = t
                .iter()
                .filter(|t| task_health(t, now) == TaskHealth::Stale)
                .count();
            let degraded = t
                .iter()
                .filter(|t| task_health(t, now) == TaskHealth::Degraded)
                .count();
            (t.len(), attention, running, failing, stale, degraded)
        })
    });

    let select = move |name: Option<String>| {
        replace_hash_param("inspect", name.as_deref());
        inspect.set(name);
    };
    let set_filter = Callback::new(move |value: TaskFilter| {
        filter.set(value);
        replace_query_param("filter", value.param());
    });

    let sentence = Signal::derive(move || {
        let (total, attention, ..) = counts.get();
        if attention == 0 {
            format!("{total} tasks, all healthy.")
        } else {
            format!(
                "{total} tasks, {attention} need{} you.",
                if attention == 1 { "s" } else { "" }
            )
        }
    });
    let lede = Signal::derive(move || {
        let (_, _, running, ..) = counts.get();
        (running > 0).then(|| {
            let names: Vec<String> = tasks.with(|t| {
                t.iter()
                    .filter(|t| t.running)
                    .map(|t| task_label(&t.name, t.display_name.as_deref()))
                    .collect()
            });
            format!("Running now: {}.", names.join(", "))
        })
    });
    let next = Memo::new(move |_| {
        tasks.with(|t| {
            t.iter()
                .filter(|t| cadence(t) != Cadence::Realtime)
                .filter_map(|t| {
                    Some((
                        next_run_ms(t)?,
                        task_label(&t.name, t.display_name.as_deref()),
                    ))
                })
                .min_by(|a, b| a.0.total_cmp(&b.0))
        })
    });
    let seg_options = Signal::derive(move || {
        let (total, attention, running, ..) = counts.get();
        vec![
            SegOption::new(TaskFilter::All, "All").with_count(total),
            SegOption::new(TaskFilter::Attention, "Attention").with_count(attention),
            SegOption::new(TaskFilter::Running, "Running").with_count(running),
        ]
    });

    let docked = Signal::derive(move || wide.get() && inspect.with(Option::is_some));
    let selected_task = Memo::new(move |_| {
        let name = inspect.get()?;
        tasks.with(|t| t.iter().find(|t| t.name == name).cloned())
    });
    // The inspector reads its task live; rebuilding it on every task update
    // would reset the selected run log and scroll.
    let selected_name =
        Memo::new(move |_| selected_task.with(|t| t.as_ref().map(|t| t.name.clone())));

    // A row is built once per task name and every cell reads its own memo:
    // re-creating the row on each snapshot or minute tick would disarm a
    // pending Run confirmation and drop keyboard focus from the row.
    let row = move |name: String| {
        let task = Memo::new({
            let name = name.clone();
            move |_| tasks.with(|t| t.iter().find(|t| t.name == name).cloned())
        });
        let runs = Memo::new({
            let name = name.clone();
            move |_| history.with(|h| h.get(&name).cloned().unwrap_or_default())
        });
        let selected = {
            let name = name.clone();
            move || inspect.with(|i| i.as_deref() == Some(name.as_str()))
        };
        let exists = Memo::new(move |_| task.with(Option::is_some));
        let health = Memo::new(move |_| {
            let now = minute.get();
            task.with(|t| t.as_ref().map_or(TaskHealth::Ok, |t| task_health(t, now)))
        });
        let label = Memo::new(move |_| {
            task.with(|t| {
                t.as_ref()
                    .map(|t| task_label(&t.name, t.display_name.as_deref()))
                    .unwrap_or_default()
            })
        });
        let realtime = Memo::new(move |_| {
            task.with(|t| t.as_ref().is_some_and(|t| cadence(t) == Cadence::Realtime))
        });
        let detail = Signal::derive(move || {
            task.with(|t| {
                t.as_ref()
                    .and_then(|t| t.last_run.as_ref().and_then(run_detail))
            })
        });
        let schedule = Memo::new(move |_| {
            task.with(|t| t.as_ref().map(|t| t.schedule.clone()).unwrap_or_default())
        });
        let last = Memo::new(move |_| task.with(|t| t.as_ref().and_then(|t| t.last_run.clone())));
        let next_ms = Memo::new(move |_| task.with(|t| t.as_ref().and_then(next_run_ms)));
        let next_list = Memo::new(move |_| {
            task.with(|t| {
                t.as_ref()
                    .map(|t| {
                        t.next_runs
                            .iter()
                            .filter_map(|n| parse_date_ms(n))
                            .collect::<Vec<f64>>()
                    })
                    .unwrap_or_default()
            })
        });
        let running = Memo::new(move |_| task.with(|t| t.as_ref().is_some_and(|t| t.running)));
        let no_runs = Memo::new(move |_| runs.with(Vec::is_empty));
        let history_label = Signal::derive(move || format!("Last {STRIP} runs of {}", label.get()));
        // The history cell on desktop and the meta line on phone.
        let history = move |phone: bool| {
            move || {
                if realtime.get() && !phone {
                    let ticks = Signal::derive(move || {
                        runs.with(|r| {
                            r.iter()
                                .map(|r| (r.started_at as f64, cell_kind(r)))
                                .collect::<Vec<_>>()
                        })
                    });
                    view! { <TimeLane now ticks scheduled=next_list label=history_label/> }
                        .into_any()
                } else if history_loaded.get() && no_runs.get() {
                    view! { <span class="small off">"No runs yet"</span> }.into_any()
                } else {
                    let slots = if phone { 8 } else { STRIP };
                    let cells = Signal::derive(move || {
                        runs.with(|r| {
                            r.iter()
                                .take(slots)
                                .rev()
                                .map(|r| RunCell {
                                    kind: cell_kind(r),
                                    title: format!(
                                        "{} · {} · {}{}",
                                        format_absolute(r.started_at as f64),
                                        run_status_label(r.status),
                                        run_duration(
                                            r,
                                            r.finished_at.unwrap_or(r.started_at) as f64
                                        ),
                                        run_detail(r)
                                            .map(|(_, s)| format!(" · {s}"))
                                            .unwrap_or_default(),
                                    ),
                                })
                                .collect::<Vec<_>>()
                        })
                    });
                    view! { <RunStrip cells slots label=history_label/> }.into_any()
                }
            }
        };
        let select_name = name.clone();
        let key_name = name.clone();
        move || {
            if !exists.get() {
                return None;
            }
            let select_name = select_name.clone();
            let key_name = key_name.clone();
            let selected = selected.clone();
            let name = name.clone();
            let mono = name.clone();
            Some(view! {
                <tr
                    data-row="true"
                    tabindex="0"
                    class={
                        let selected = selected.clone();
                        move || if selected() { "clickable selected" } else { "clickable" }
                    }
                    aria-selected=move || selected().to_string()
                    on:click=move |_| select(Some(select_name.clone()))
                    on:keydown=move |ev: web_sys::KeyboardEvent| {
                        if ev.key() == "Enter" && ev.target() == ev.current_target() {
                            select(Some(key_name.clone()));
                        }
                    }
                >
                    <td class="cell-status">
                        {move || {
                            let health = health.get();
                            let status_title = (health == TaskHealth::Degraded)
                                .then(|| detail.get().map(|d| d.1))
                                .flatten();
                            view! { <Status kind=health_kind(health) label=health_word(health) title=status_title dot_only=health == TaskHealth::Ok/> }
                        }}
                    </td>
                    <td class="grow">
                        <div class=move || if running.get() { "task-name text-signal" } else { "task-name" }>{move || label.get()}</div>
                        <div class="row-sub ops-task-sub">
                            <span class="mono">{mono}</span>
                            {move || detail.get().map(|(tone, s)| view! {
                                <span class="ops-sep">" · "</span>
                                <span class=format!("ops-detail {}", detail_class(tone))>{s}</span>
                            })}
                        </div>
                        <div class="ops-phone-meta">
                            {history(true)}
                            {move || last.get().map(|r| {
                                let started = r.started_at as f64;
                                view! { <span class="num dim ops-last">{move || format_relative_at(started, now.get())}</span> }
                            })}
                        </div>
                    </td>
                    <td class="hide-phone hide-below-wide nowrap" title=move || schedule.get()>
                        {move || schedule.with(|s| describe_cron(s).unwrap_or_else(|| s.clone()))}
                    </td>
                    <td class="hide-phone nowrap">
                        {move || last.get().map(|r| {
                            let started = r.started_at as f64;
                            view! {
                                <div class="num" title=format_absolute(started)>{move || format_relative_at(started, now.get())}</div>
                                <div class="small dim num">{move || run_duration(&r, now.get())}</div>
                            }
                        })}
                    </td>
                    <td class="hide-phone">{history(false)}</td>
                    <td class="numeric num">
                        {move || if running.get() { "running".to_owned() } else { next_ms.get().map(|n| format_next(n - now.get())).unwrap_or_else(|| "—".to_owned()) }}
                    </td>
                    <td class="cell-action" on:click=|ev| ev.stop_propagation()>
                        <RunButton task=name running size=ButtonSize::Sm/>
                    </td>
                </tr>
            })
        }
    };

    view! {
        <PageHead title=sentence lede sentence=true/>
        {move || live.error.get().map(|e| view! {
            <p class="inline-note warn" role="status">{format!("Refresh failed ({e}); showing the last known state.")}</p>
        })}
        <ReadoutBand cols=5 aria_label="Task health">
            <Readout label="Healthy" value=Signal::derive(move || {
                let (total, attention, ..) = counts.get();
                (total - attention).to_string()
            }) unit=Signal::derive(move || Some(format!("/{}", counts.get().0)))/>
            <Readout label="Running" value=Signal::derive(move || counts.get().2.to_string()) tone=Signal::derive(move || if counts.get().2 > 0 { Tone::Signal } else { Tone::Neutral })/>
            <Readout label="Failing" value=Signal::derive(move || counts.get().3.to_string()) tone=Signal::derive(move || if counts.get().3 > 0 { Tone::Fault } else { Tone::Neutral })/>
            <Readout
                label="Warnings"
                value=Signal::derive(move || { let c = counts.get(); (c.4 + c.5).to_string() })
                tone=Signal::derive(move || { let c = counts.get(); if c.4 + c.5 > 0 { Tone::Warn } else { Tone::Neutral } })
            >
                {move || { let c = counts.get(); format!("{} degraded · {} stale", c.5, c.4) }}
            </Readout>
            <Readout label="Next run" value=Signal::derive(move || next.get().map(|(t, _)| format_next(t - now.get())).unwrap_or_else(|| "—".to_owned()))>
                {move || next.get().map(|(_, name)| view! { <span class="truncate">{name}</span> })}
            </Readout>
        </ReadoutBand>
        <div class=move || if docked.get() { "split docked ops-layout" } else { "ops-layout" }>
            <div class="stack-lg">
                <div>
                <div class="toolbar">
                    <SearchField
                        value=query
                        on_input=Callback::new(move |v| query.set(v))
                        placeholder="Filter tasks"
                        aria_label="Filter tasks"
                        shortcut=true
                    />
                    <Segmented options=seg_options value=filter on_change=set_filter aria_label="Task status"/>
                </div>
                {move || {
                    if no_groups.get() {
                        return view! {
                            <EmptyState message="No tasks match this view." action=ViewFn::from(move || view! {
                                <Button size=ButtonSize::Sm variant=ButtonVariant::Ghost on_click=Callback::new(move |_| {
                                    query.set(String::new());
                                    set_filter.run(TaskFilter::All);
                                })>"Clear filter"</Button>
                            })/>
                        }
                        .into_any();
                    }
                    view! {
                        <div class="table-wrap panel">
                            <table class="table ops-table" data-primary-rows="true">
                                <thead>
                                    <tr>
                                        <th><span class="sr-only">"Status"</span></th>
                                        <th class="grow">"Task"</th>
                                        <th class="hide-phone hide-below-wide">"Cadence"</th>
                                        <th class="hide-phone">"Last run"</th>
                                        <th class="hide-phone">"History"</th>
                                        <th class="numeric">"Next"</th>
                                        <th><span class="sr-only">"Run"</span></th>
                                    </tr>
                                </thead>
                                <For
                                    each=move || groups.with(|g| g.iter().map(|(group, _)| *group).collect::<Vec<_>>())
                                    key=|group| *group
                                    children=move |group| {
                                        let names = Memo::new(move |_| {
                                            groups.with(|g| {
                                                g.iter()
                                                    .find(|(c, _)| *c == group)
                                                    .map(|(_, names)| names.clone())
                                                    .unwrap_or_default()
                                            })
                                        });
                                        view! {
                                            <tbody>
                                                <tr class="group-row"><th colspan="7" scope="colgroup">{group.label()} " " <span class="num off">{move || names.with(Vec::len)}</span></th></tr>
                                                <For each=move || names.get() key=|n| n.clone() children=row/>
                                            </tbody>
                                        }
                                    }
                                />
                            </table>
                        </div>
                    }
                    .into_any()
                }}
                </div>
                <Panel title="Run log" pad=false>
                    <div class="panel-body">
                        <RunLog
                            snapshot=Signal::derive(move || live.snapshot.get().unwrap_or_else(empty_snapshot))
                            on_select=Callback::new(move |run: Run| log_run.set(Some(run)))
                            selected_run=Signal::derive(move || log_run.with(|r| r.as_ref().map(|r| r.run_id.clone())))
                        />
                    </div>
                </Panel>
            </div>
            {move || selected_name.get().and_then(|_| selected_task.get_untracked()).map(|task| view! {
                <TaskInspector
                    task
                    runs=Signal::derive(move || inspect.with(|n| n.as_ref().and_then(|n| history.with(|h| h.get(n).cloned()))).unwrap_or_default())
                    docked
                    on_close=Callback::new(move |()| select(None))
                    on_expand=Callback::new(move |run: Run| log_run.set(Some(run)))
                />
            })}
        </div>
        {move || log_run.get().map(|run| view! {
            <LogViewer run on_close=Callback::new(move |()| log_run.set(None))/>
        })}
    }
}

#[component]
fn TaskInspector(
    task: TaskInfo,
    #[prop(into)] runs: Signal<Vec<Run>>,
    #[prop(into)] docked: Signal<bool>,
    on_close: Callback<()>,
    on_expand: Callback<Run>,
) -> impl IntoView {
    let live = use_live_data();
    let now = use_now(1000);
    let name = task.name.clone();
    let task_memo = Memo::new({
        let name = name.clone();
        move |_| {
            live.snapshot.with(|s| {
                s.as_ref()
                    .and_then(|s| s.tasks.iter().find(|t| t.name == name).cloned())
            })
        }
    });
    let shown = RwSignal::new(None::<String>);
    let current_run = Memo::new(move |_| {
        let runs = runs.get();
        match shown.get() {
            Some(id) => runs.into_iter().find(|r| r.run_id == id),
            None => runs.into_iter().next(),
        }
    });
    let running = Signal::derive(move || task_memo.with(|t| t.as_ref().is_some_and(|t| t.running)));
    let title = task_label(&task.name, task.display_name.as_deref());
    let status_name = name.clone();
    let status = ViewFn::from(move || {
        let health = task_memo.with(|t| t.as_ref().map(|t| task_health(t, now.get_untracked())));
        let group = cadence_label(&status_name, live);
        view! {
            {health.map(|h| view! { <Status kind=health_kind(h) label=health_word(h)/> })}
            <Tag>{group}</Tag>
        }
    });
    let run_name = name.clone();
    let actions = ViewFn::from(move || {
        view! {
            <RunButton task=run_name.clone() running primary=true/>
        }
    });
    let history_id = format!("task-history-{name}");
    view! {
        <Inspector title=title status actions on_close docked>
            <section class="inspector-section">
                <dl class="kv">
                    <dt>"Schedule"</dt>
                    <dd>
                        {describe_cron(&task.schedule).unwrap_or_else(|| task.schedule.clone())}
                        <span class="mono off">" " {task.schedule.clone()}</span>
                    </dd>
                    <dt>"Next runs"</dt>
                    <dd class="num">
                        {move || task_memo.with(|t| t.as_ref().map(|t| {
                            t.next_runs
                                .iter()
                                .filter_map(|n| parse_date_ms(n))
                                .map(|n| format!("{} ({})", format_absolute(n), format_next(n - now.get())))
                                .collect::<Vec<_>>()
                                .join(" · ")
                        }))}
                    </dd>
                    {move || task_memo.with(|t| t.as_ref().and_then(|t| t.last_run.clone())).map(|r| view! {
                        <dt>"Last run"</dt>
                        <dd class="num">{format!("{} · {}", format_absolute(r.started_at as f64), run_duration(&r, now.get()))}</dd>
                        <dt>"Trigger"</dt>
                        <dd><TriggerBadge trigger=r.trigger/></dd>
                        <dt>"Run id"</dt>
                        <dd class="mono small truncate" title=r.run_id.clone()>{r.run_id.clone()}</dd>
                    })}
                </dl>
            </section>
            {match name.as_str() {
                omni_web_pages::calendar::CALENDAR_TASK => Some(view! {
                    <section class="inspector-section">
                        <h3 class="label">"Calendar sync"</h3>
                        <omni_web_pages::CalendarSyncFacts/>
                    </section>
                }.into_any()),
                _ => None,
            }}
            <section class="inspector-section">
                <h3 class="label">"History"</h3>
                <div class="rows" id=history_id.clone()>
                    {move || {
                        let list: Vec<Run> = runs.get().into_iter().take(STRIP).collect();
                        if list.is_empty() {
                            return view! { <EmptyState compact=true message="No runs yet."/> }.into_any();
                        }
                        list.into_iter().map(|run| {
                            let id = run.run_id.clone();
                            let is_shown = {
                                let id = id.clone();
                                move || current_run.with(|c| c.as_ref().is_some_and(|c| c.run_id == id))
                            };
                            let kind = run_status_kind(run.status);
                            let word = run_status_label(run.status);
                            let detail = run_detail(&run);
                            let started = run.started_at as f64;
                            let duration = run_duration(&run, now.get_untracked());
                            view! {
                                <div class=move || if is_shown() { "row dense selected" } else { "row dense" }>
                                    <Status kind label=word dot_only=kind == StatusKind::Ok/>
                                    <span class="row-main">
                                        <span class="row-title num">{format_relative_at(started, now.get_untracked())} " · " {duration}</span>
                                        {detail.map(|(tone, d)| view! { <span class=format!("row-sub run-detail {}", detail_class(tone))>{d}</span> })}
                                    </span>
                                    <span class="row-end">
                                        <Button
                                            size=ButtonSize::Sm
                                            variant=ButtonVariant::Ghost
                                            icon=Icon::Logs
                                            aria_label="Show this run's log"
                                            on_click=Callback::new(move |_| shown.set(Some(id.clone())))
                                        >
                                            "Logs"
                                        </Button>
                                    </span>
                                </div>
                            }
                        }).collect_view().into_any()
                    }}
                </div>
            </section>
            <section class="inspector-section">
                <div class="cluster">
                    <h3 class="label">"Log"</h3>
                    <span class="spacer"></span>
                    {move || current_run.get().map(|run| view! {
                        <Button size=ButtonSize::Sm variant=ButtonVariant::Ghost on_click=Callback::new(move |_| on_expand.run(run.clone()))>
                            "Expand"
                        </Button>
                    })}
                </div>
                {move || match current_run.get() {
                    Some(run) => view! { <LogWell run bounded=true/> }.into_any(),
                    None => view! { <p class="log-note">"No run to show yet."</p> }.into_any(),
                }}
            </section>
        </Inspector>
    }
}

fn cadence_label(name: &str, live: omni_web_kit::live::LiveData) -> &'static str {
    live.snapshot.with_untracked(|s| {
        s.as_ref()
            .and_then(|s| s.tasks.iter().find(|t| t.name == name))
            .map_or("Scheduled", |t| cadence(t).label())
    })
}

pub(crate) fn empty_snapshot() -> api::Snapshot {
    api::Snapshot {
        tasks: Vec::new(),
        streamers: Vec::new(),
        runs: Vec::new(),
        on_deck: Vec::new(),
        build: omni_api::build::BuildIdentity::default(),
    }
}

#[cfg(test)]
mod tests {
    use omni_api::runs::RunTrigger;

    use super::*;

    fn task(name: &str, next: [&str; 2], running: bool) -> TaskInfo {
        TaskInfo {
            name: name.into(),
            display_name: None,
            schedule: String::new(),
            running,
            next_runs: next.iter().map(|s| (*s).to_owned()).collect(),
            last_run: None,
        }
    }

    fn run(id: &str, task: &str, started: i64) -> Run {
        Run {
            run_id: id.into(),
            task_name: task.into(),
            trigger: RunTrigger::Schedule,
            scheduled_for: None,
            started_at: started,
            finished_at: Some(started + 10),
            status: RunStatus::Success,
            error: None,
            summary: None,
        }
    }

    #[test]
    fn tasks_group_by_cadence_and_filter() {
        let tasks = [
            task(
                "LiveCheck",
                ["2026-01-01T00:00:00.000Z", "2026-01-01T00:00:20.000Z"],
                false,
            ),
            task(
                "Pets",
                ["2026-01-01T01:00:00.000Z", "2026-01-01T02:00:00.000Z"],
                true,
            ),
            task(
                "Recs",
                ["2026-01-02T00:00:00.000Z", "2026-01-03T00:00:00.000Z"],
                false,
            ),
        ];
        let groups = grouped_tasks(&tasks, TaskFilter::All, "", 0.0);
        assert_eq!(
            groups,
            [
                (Cadence::Realtime, vec!["LiveCheck".to_owned()]),
                (Cadence::Frequent, vec!["Pets".to_owned()]),
                (Cadence::Scheduled, vec!["Recs".to_owned()]),
            ]
        );
        let running = grouped_tasks(&tasks, TaskFilter::Running, "", 0.0);
        assert_eq!(running, [(Cadence::Frequent, vec!["Pets".to_owned()])]);
        assert!(grouped_tasks(&tasks, TaskFilter::All, "zzz", 0.0).is_empty());
        assert_eq!(
            TaskFilter::from_param(Some("attention")),
            TaskFilter::Attention
        );
    }

    #[test]
    fn history_merges_without_duplicates() {
        let merged = merge_history(
            &[run("a", "T", 1), run("b", "T", 2)],
            &[run("b", "T", 2), run("c", "T", 3)],
        );
        let ids: Vec<&str> = merged["T"].iter().map(|r| r.run_id.as_str()).collect();
        assert_eq!(ids, ["c", "b", "a"]);
    }
}
