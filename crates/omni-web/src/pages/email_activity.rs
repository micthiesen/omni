//! Email: what the parcel and calendar pipelines did with each email, and
//! the sender rules (`Activity | Rules`).

use std::collections::HashMap;

use leptos::prelude::*;
use omni_api::email::{
    BuiltinRules, EmailActivity, EmailActivityOutcome, EmailFeedback, EmailPipelineName, EmailRule,
    EmailRuleInput, RuleScope, RuleUpsertStatus, RuleVerdict,
};
use omni_web_kit::api;
use omni_web_kit::components::email_inspector::outcome_kind;
use omni_web_kit::components::{
    Button, ButtonSize, ButtonVariant, ConfirmButton, Disclosure, EmailInspector, EmptyState,
    ErrorState, PageHead, Panel, Readout, ReadoutBand, SearchField, SegOption, Segmented,
    ShowMoreButton, SkeletonRows, Status, Tag, ToastKind, Tone, use_show_more, use_toast,
};
use omni_web_kit::hooks::{hash_param, query_param, replace_query_param, use_is_wide};
use omni_web_kit::task::{spawn_detached, spawn_scoped};
use omni_web_kit::utils::email_labels::{PIPELINES, outcome_label, pipeline_label};
use omni_web_kit::utils::format::format_cents;
use omni_web_kit::utils::js::{
    date_locale_date_string, date_locale_time_string, local_day_index, now_ms,
};

const OUTCOME_ORDER: [EmailActivityOutcome; 7] = [
    EmailActivityOutcome::Processed,
    EmailActivityOutcome::Partial,
    EmailActivityOutcome::Failed,
    EmailActivityOutcome::Error,
    EmailActivityOutcome::NoMatches,
    EmailActivityOutcome::Skipped,
    EmailActivityOutcome::Filtered,
];
const PAGE: usize = 50;

fn scope_label(scope: RuleScope) -> &'static str {
    match scope {
        RuleScope::Parcel => "Parcels",
        RuleScope::Calendar => "Calendar",
        RuleScope::Both => "Both",
    }
}

fn verdict_tone(verdict: RuleVerdict) -> Tone {
    match verdict {
        RuleVerdict::Block => Tone::Fault,
        RuleVerdict::Allow => Tone::Ok,
    }
}

fn verdict_label(verdict: RuleVerdict) -> &'static str {
    match verdict {
        RuleVerdict::Block => "block",
        RuleVerdict::Allow => "allow",
    }
}

/// Matches subject, sender and items.
pub fn matches_query(activity: &EmailActivity, query: &str) -> bool {
    let q = query.trim().to_lowercase();
    q.is_empty()
        || activity.subject.to_lowercase().contains(&q)
        || activity.from.to_lowercase().contains(&q)
        || activity.items.iter().any(|i| i.to_lowercase().contains(&q))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tab {
    Activity,
    Rules,
}

#[component]
fn BuiltinList(label: &'static str, verdict: RuleVerdict, patterns: Vec<String>) -> impl IntoView {
    let count = patterns.len();
    view! {
        <Disclosure summary=label meta=count.to_string()>
            <div class="chips">
                <Tag tone=verdict_tone(verdict)>{verdict_label(verdict)}</Tag>
                {patterns.into_iter().map(|p| view! { <code class="tag">{p}</code> }).collect_view()}
            </div>
        </Disclosure>
    }
}

#[component]
fn SenderRules() -> impl IntoView {
    let rules = RwSignal::new(None::<Vec<EmailRule>>);
    let builtin = RwSignal::new(None::<BuiltinRules>);
    let error = RwSignal::new(None::<String>);
    let pattern = RwSignal::new(String::new());
    let scope = RwSignal::new(RuleScope::Parcel);
    let verdict = RwSignal::new(RuleVerdict::Block);
    let submitting = RwSignal::new(false);
    let deleting = RwSignal::new(None::<String>);
    let reload = RwSignal::new(0u32);
    let toast = use_toast();

    Effect::new(move |_| {
        reload.track();
        spawn_scoped(async move {
            match api::fetch_email_rules().await {
                Ok(res) => {
                    rules.set(Some(res.rules));
                    builtin.set(Some(res.builtin));
                    error.set(None);
                }
                Err(e) => error.set(Some(e.message().to_owned())),
            }
        });
    });

    let add_rule = move || {
        let trimmed = pattern.get_untracked().trim().to_lowercase();
        if trimmed.is_empty() || submitting.get_untracked() {
            return;
        }
        submitting.set(true);
        error.set(None);
        let input = EmailRuleInput {
            pattern: trimmed,
            scope: scope.get_untracked(),
            verdict: verdict.get_untracked(),
        };
        spawn_detached(async move {
            match api::create_email_rule(&input).await {
                Ok(res) => {
                    let (message, kind) = match res.status {
                        RuleUpsertStatus::Created => ("Rule added".to_owned(), ToastKind::Info),
                        RuleUpsertStatus::Merged => (
                            res.message
                                .clone()
                                .unwrap_or_else(|| "Merged into a single Both rule".into()),
                            ToastKind::Info,
                        ),
                        RuleUpsertStatus::Exists => (
                            res.message
                                .clone()
                                .unwrap_or_else(|| "That rule already exists".into()),
                            ToastKind::Error,
                        ),
                        RuleUpsertStatus::Builtin => (
                            res.message
                                .clone()
                                .unwrap_or_else(|| "Already covered by a built-in list".into()),
                            ToastKind::Error,
                        ),
                    };
                    toast.show(message, kind);
                    // builtin/exists may return no new rule; prepend only when one landed.
                    if let Some(rule) = res.rule {
                        rules.update(|list| {
                            let mut others: Vec<EmailRule> = list
                                .take()
                                .unwrap_or_default()
                                .into_iter()
                                .filter(|r| r.rule_id != rule.rule_id)
                                .collect();
                            others.insert(0, rule);
                            *list = Some(others);
                        });
                    }
                    pattern.set(String::new());
                }
                Err(e) => error.set(Some(e.message().to_owned())),
            }
            submitting.set(false);
        });
    };

    let remove_rule = move |rule: EmailRule| {
        deleting.set(Some(rule.rule_id.clone()));
        error.set(None);
        spawn_detached(async move {
            match api::delete_email_rule(&rule.rule_id).await {
                Ok(_) => rules.update(|list| {
                    if let Some(list) = list {
                        list.retain(|r| r.rule_id != rule.rule_id);
                    }
                }),
                Err(e) => error.set(Some(e.message().to_owned())),
            }
            deleting.set(None);
        });
    };

    view! {
        <div class="stack-lg">
            <Panel title="Your rules" head_end=ViewFn::from(move || view! {
                <span class="panel-meta">{move || rules.with(|r| r.as_ref().map(|r| r.len().to_string()))}</span>
            })>
                <form
                    class="toolbar rule-form panel-body"
                    on:submit=move |event| {
                        event.prevent_default();
                        add_rule();
                    }
                >
                    <input
                        type="text"
                        class="input grow"
                        placeholder="x@y.com or y.com"
                        prop:value=move || pattern.get()
                        on:input=move |ev| pattern.set(event_target_value(&ev))
                        aria-label="Sender pattern"
                    />
                    <select
                        class="select"
                        prop:value=move || scope.get().as_str()
                        on:change=move |ev| {
                            scope.set(match event_target_value(&ev).as_str() {
                                "calendar" => RuleScope::Calendar,
                                "both" => RuleScope::Both,
                                _ => RuleScope::Parcel,
                            })
                        }
                        aria-label="Rule scope"
                    >
                        <option value="parcel">"Parcels"</option>
                        <option value="calendar">"Calendar"</option>
                        <option value="both">"Both"</option>
                    </select>
                    <select
                        class="select"
                        prop:value=move || verdict.get().as_str()
                        on:change=move |ev| {
                            verdict.set(if event_target_value(&ev) == "allow" { RuleVerdict::Allow } else { RuleVerdict::Block })
                        }
                        aria-label="Rule verdict"
                    >
                        <option value="block">"Block"</option>
                        <option value="allow">"Allow"</option>
                    </select>
                    <Button
                        submit=true
                        variant=ButtonVariant::Primary
                        busy=submitting
                        disabled=Signal::derive(move || pattern.with(|p| p.trim().is_empty()))
                        disabled_reason="Enter an address or domain first"
                    >
                        "Add"
                    </Button>
                </form>
                {move || error.get().map(|e| view! {
                    <ErrorState title="Rules could not be updated" raw=e retry=Callback::new(move |()| reload.update(|n| *n += 1))/>
                })}
                {move || match rules.get() {
                    None => error.with(Option::is_none).then(|| view! { <SkeletonRows count=3/> }.into_any()),
                    Some(list) if list.is_empty() => Some(view! {
                        <EmptyState compact=true message="No sender rules yet. Rules match a full address (x@y.com) or a domain (y.com)."/>
                    }.into_any()),
                    Some(list) => Some(view! {
                        <div class="rows">
                            {list.into_iter().map(|rule| {
                                let target = rule.clone();
                                let id = rule.rule_id.clone();
                                view! {
                                    <div class="row dense">
                                        <Tag tone=verdict_tone(rule.verdict)>{verdict_label(rule.verdict)}</Tag>
                                        <span class="row-main"><span class="row-title mono truncate" title=rule.pattern.clone()>{rule.pattern.clone()}</span></span>
                                        <span class="row-end">
                                            <span class="small dim">{scope_label(rule.scope)}</span>
                                            <ConfirmButton
                                                label="Delete"
                                                destructive=true
                                                confirm_label="Delete rule"
                                                variant=ButtonVariant::Ghost
                                                size=ButtonSize::Sm
                                                busy=Signal::derive(move || deleting.get().as_ref() == Some(&id))
                                                on_confirm=Callback::new(move |()| remove_rule(target.clone()))
                                            />
                                        </span>
                                    </div>
                                }
                            }).collect_view()}
                        </div>
                    }.into_any()),
                }}
            </Panel>
            {move || builtin.get().map(|b| view! {
                <Panel title="Built-in lists" pad=true>
                    <p class="small dim">"Built-ins ship with the app. An Allow rule above overrides a built-in block for that sender."</p>
                    <BuiltinList label="Parcels · blocked" verdict=RuleVerdict::Block patterns=b.parcel.blocked/>
                    <BuiltinList label="Parcels · auto-pass" verdict=RuleVerdict::Allow patterns=b.parcel.auto_pass/>
                    <BuiltinList label="Calendar · blocked" verdict=RuleVerdict::Block patterns=b.calendar.blocked/>
                    <BuiltinList label="Calendar · auto-pass" verdict=RuleVerdict::Allow patterns=b.calendar.auto_pass/>
                </Panel>
            })}
        </div>
    }
}

fn day_label(day: i64, today: i64, ms: f64) -> String {
    match today - day {
        0 => "Today".to_owned(),
        1 => "Yesterday".to_owned(),
        _ => date_locale_date_string(
            ms,
            &[("weekday", "short"), ("month", "short"), ("day", "numeric")],
        ),
    }
}

#[component]
pub fn EmailActivityPage() -> impl IntoView {
    let wide = use_is_wide();
    let tab = RwSignal::new(Tab::Activity);
    let activities = RwSignal::new(None::<Vec<EmailActivity>>);
    let error = RwSignal::new(None::<String>);
    let reload = RwSignal::new(0u32);
    let pipeline = RwSignal::new(None::<EmailPipelineName>);
    let outcome = RwSignal::new(query_param("outcome").unwrap_or_default());
    let query = RwSignal::new(String::new());
    let feedback = RwSignal::new(HashMap::<String, EmailFeedback>::new());
    let selected = RwSignal::new(None::<EmailActivity>);

    Effect::new(move |_| {
        let p = pipeline.get();
        reload.track();
        error.set(None);
        spawn_scoped(async move {
            match api::fetch_email_activity(p, Some(500)).await {
                Ok(res) => {
                    // `#inspect=<activityId>` .
                    if selected.with_untracked(Option::is_none)
                        && let Some(id) = hash_param("inspect")
                        && let Some(hit) = res.activities.iter().find(|a| a.activity_id == id)
                    {
                        selected.set(Some(hit.clone()));
                    }
                    activities.set(Some(res.activities));
                }
                Err(e) => error.set(Some(e.message().to_owned())),
            }
        });
    });
    // Feedback indicators are decorative; the page works without them.
    spawn_scoped(async move {
        if let Ok(res) = api::fetch_email_feedback().await {
            feedback.set(
                res.feedback
                    .into_iter()
                    .map(|f| (f.activity_id.clone(), f))
                    .collect(),
            );
        }
    });

    let today_stats = Memo::new(move |_| {
        let since = now_ms() - 86_400_000.0;
        activities.with(|a| {
            a.as_ref().map(|a| {
                let today: Vec<&EmailActivity> = a
                    .iter()
                    .filter(|x| x.processed_at as f64 >= since)
                    .collect();
                let processed = today
                    .iter()
                    .filter(|x| {
                        matches!(
                            x.outcome,
                            EmailActivityOutcome::Processed | EmailActivityOutcome::Partial
                        )
                    })
                    .count();
                let filtered = today
                    .iter()
                    .filter(|x| {
                        matches!(
                            x.outcome,
                            EmailActivityOutcome::Filtered
                                | EmailActivityOutcome::Skipped
                                | EmailActivityOutcome::NoMatches
                        )
                    })
                    .count();
                let failed = today
                    .iter()
                    .filter(|x| {
                        matches!(
                            x.outcome,
                            EmailActivityOutcome::Failed | EmailActivityOutcome::Error
                        )
                    })
                    .count();
                let cost: f64 = today.iter().filter_map(|x| x.cost_cents).sum();
                (today.len(), processed, filtered, failed, cost)
            })
        })
    });
    let counts = Memo::new(move |_| {
        let mut counts: HashMap<&'static str, usize> = HashMap::new();
        activities.with(|list| {
            for a in list.iter().flatten() {
                *counts.entry(a.outcome.as_str()).or_insert(0) += 1;
            }
        });
        counts
    });
    let filtered = Memo::new(move |_| {
        let o = outcome.get();
        let q = query.get();
        activities.with(|list| {
            list.iter()
                .flatten()
                .filter(|a| o.is_empty() || a.outcome.as_str() == o)
                .filter(|a| matches_query(a, &q))
                .cloned()
                .collect::<Vec<_>>()
        })
    });
    let shown = use_show_more(
        Signal::derive(move || filtered.get()),
        PAGE,
        Signal::derive(move || {
            format!(
                "{}:{}:{}",
                pipeline.get().map_or("all", |p| p.as_str()),
                outcome.get(),
                query.get()
            )
        }),
    );

    let on_activity_change = Callback::new(move |updated: EmailActivity| {
        activities.update(|list| {
            if let Some(list) = list {
                for a in list.iter_mut() {
                    if a.activity_id == updated.activity_id {
                        *a = updated.clone();
                    }
                }
            }
        });
    });
    let on_feedback_change =
        Callback::new(move |(id, updated): (String, Option<EmailFeedback>)| {
            feedback.update(|map| match updated {
                Some(f) => {
                    map.insert(id, f);
                }
                None => {
                    map.remove(&id);
                }
            });
        });

    let sentence = Signal::derive(move || match today_stats.get() {
        None => "Email".to_owned(),
        Some((total, _, _, failed, _)) => format!(
            "{total} screened today, {}.",
            match failed {
                0 => "none failed".to_owned(),
                1 => "1 failed".to_owned(),
                n => format!("{n} failed"),
            }
        ),
    });
    let pipeline_options = Signal::derive(|| {
        std::iter::once(SegOption::new(None, "All"))
            .chain(
                PIPELINES
                    .into_iter()
                    .map(|p| SegOption::new(Some(p), pipeline_label(p))),
            )
            .collect::<Vec<_>>()
    });
    let outcome_options = Signal::derive(move || {
        let counts = counts.get();
        let total = activities.with(|a| a.as_ref().map_or(0, Vec::len));
        std::iter::once(SegOption::new(String::new(), "All").with_count(total))
            .chain(OUTCOME_ORDER.iter().filter_map(|o| {
                let n = counts.get(o.as_str()).copied()?;
                Some(SegOption::new(o.as_str().to_owned(), outcome_label(*o)).with_count(n))
            }))
            .collect::<Vec<_>>()
    });
    let set_outcome = Callback::new(move |value: String| {
        replace_query_param("outcome", (!value.is_empty()).then_some(value.as_str()));
        outcome.set(value);
    });
    let docked = Signal::derive(move || wide.get() && selected.with(Option::is_some));
    // The list closure reads only these shapes; the rows read `shown`
    // themselves, so an inspector edit or a refetch does not rebuild the
    // outcome filter or the table shell.
    let list_state = Memo::new(move |_| activities.with(|a| a.as_ref().map(Vec::is_empty)));
    let none_match = Memo::new(move |_| filtered.with(Vec::is_empty));

    let table = move || {
        let rows = shown.visible.get();
        let today = local_day_index(now_ms());
        let mut last_day = None;
        let mut out = Vec::new();
        for activity in rows {
            let day = local_day_index(activity.processed_at as f64);
            if last_day != Some(day) {
                last_day = Some(day);
                out.push(view! {
                    <tr class="group-row"><th colspan="5" scope="colgroup">{day_label(day, today, activity.processed_at as f64)}</th></tr>
                }.into_any());
            }
            let id = activity.activity_id.clone();
            let fb_id = id.clone();
            let sel_id = id.clone();
            let clicked = activity.clone();
            let key_clicked = activity.clone();
            let subject = if activity.subject.is_empty() {
                "(no subject)".to_owned()
            } else {
                activity.subject.clone()
            };
            let is_selected =
                move || selected.with(|s| s.as_ref().is_some_and(|s| s.activity_id == sel_id));
            let selected_class = is_selected.clone();
            out.push(view! {
                <tr
                    data-row="true"
                    tabindex="0"
                    class=move || if selected_class() { "clickable selected" } else { "clickable" }
                    aria-selected=move || is_selected().to_string()
                    on:click=move |_| selected.set(Some(clicked.clone()))
                    on:keydown=move |ev: web_sys::KeyboardEvent| {
                        if ev.key() == "Enter" {
                            selected.set(Some(key_clicked.clone()));
                        }
                    }
                >
                    <td class="cell-status"><Status kind=outcome_kind(activity.outcome) label=outcome_label(activity.outcome) dot_only=activity.outcome == EmailActivityOutcome::Processed/></td>
                    <td class="grow">
                        <div class="row-title truncate" title=activity.subject.clone()>{subject}</div>
                        <div class="row-sub truncate">
                            {activity.from.clone()}
                            {activity.items.first().map(|i| view! { " · " <span class="mono">{i.clone()}</span> })}
                        </div>
                    </td>
                    <td class="hide-phone"><Tag>{pipeline_label(activity.pipeline)}</Tag></td>
                    <td class="hide-phone">
                        {move || feedback.with(|f| f.contains_key(&fb_id)).then(|| view! { <Tag tone=Tone::Info>"feedback"</Tag> })}
                    </td>
                    <td class="numeric num dim">{date_locale_time_string(activity.processed_at as f64, &[("hour", "numeric"), ("minute", "2-digit")])}</td>
                </tr>
            }.into_any());
        }
        out
    };

    view! {
        <PageHead title=sentence sentence=true actions=ViewFn::from(move || view! {
            <Segmented
                options=Signal::derive(|| vec![SegOption::new(Tab::Activity, "Activity"), SegOption::new(Tab::Rules, "Rules")])
                value=tab
                on_change=Callback::new(move |t| tab.set(t))
                aria_label="Email view"
            />
        })/>
        {move || match tab.get() {
            Tab::Rules => view! { <SenderRules/> }.into_any(),
            Tab::Activity => view! {
                {move || today_stats.get().map(|(_, processed, filtered_n, failed, cost)| view! {
                    <ReadoutBand cols=4 aria_label="Last 24 hours">
                        <Readout label="Processed · 24 h" value=processed.to_string()/>
                        <Readout label="Filtered" value=filtered_n.to_string()/>
                        <Readout label="Failed" value=failed.to_string() tone={if failed > 0 { Tone::Fault } else { Tone::Neutral }}/>
                        <Readout label="LLM cost" value=format_cents(Some(cost)).unwrap_or_else(|| "—".to_owned())/>
                    </ReadoutBand>
                })}
                <div class=move || if docked.get() { "split docked" } else { "" }>
                    <div class="stack">
                        <div class="toolbar">
                            <Segmented options=pipeline_options value=pipeline on_change=Callback::new(move |p| pipeline.set(p)) aria_label="Pipeline" small=true/>
                            <SearchField value=query on_input=Callback::new(move |v| query.set(v)) placeholder="Filter subject, sender or item" aria_label="Filter emails" shortcut=true/>
                        </div>
                        {move || (list_state.get() == Some(false)).then(|| view! {
                            <Segmented options=outcome_options value=outcome on_change=set_outcome aria_label="Outcome" small=true/>
                        })}
                        {move || {
                            match list_state.get() {
                                None => match error.get() {
                                    None => view! { <SkeletonRows count=8/> }.into_any(),
                                    Some(e) => view! {
                                        <ErrorState title="Email activity could not load" raw=e retry=Callback::new(move |()| reload.update(|n| *n += 1)) page=true/>
                                    }.into_any(),
                                },
                                Some(true) => view! { <EmptyState message="No email activity yet. Activity appears here as new emails are processed."/> }.into_any(),
                                Some(false) if none_match.get() => view! {
                                    <EmptyState
                                        message="No emails match these filters."
                                        action=ViewFn::from(move || view! {
                                            <Button size=ButtonSize::Sm variant=ButtonVariant::Ghost on_click=Callback::new(move |_| {
                                                query.set(String::new());
                                                set_outcome.run(String::new());
                                            })>"Clear filter"</Button>
                                        })
                                    />
                                }.into_any(),
                                Some(false) => view! {
                                    <div class="table-wrap panel">
                                        <table class="table" data-primary-rows="true">
                                            <thead><tr>
                                                <th><span class="sr-only">"Outcome"</span></th>
                                                <th class="grow">"Email"</th>
                                                <th class="hide-phone">"Pipeline"</th>
                                                <th class="hide-phone"><span class="sr-only">"Feedback"</span></th>
                                                <th class="numeric">"Time"</th>
                                            </tr></thead>
                                            <tbody>{table}</tbody>
                                        </table>
                                    </div>
                                    {move || shown.has_more.get().then(|| view! {
                                        <ShowMoreButton remaining=shown.remaining noun="emails" on_click=Callback::new(move |()| shown.show_more())/>
                                    })}
                                }.into_any(),
                            }
                        }}
                    </div>
                    {move || selected.get().map(|activity| {
                        let fb = feedback.with_untracked(|f| f.get(&activity.activity_id).cloned());
                        let on_close = Callback::new(move |()| selected.set(None));
                        match fb {
                            Some(feedback) => view! { <EmailInspector activity feedback docked on_activity_change on_feedback_change on_close/> }.into_any(),
                            None => view! { <EmailInspector activity docked on_activity_change on_feedback_change on_close/> }.into_any(),
                        }
                    })}
                </div>
            }.into_any(),
        }}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_matches_subject_sender_and_items() {
        let activity = EmailActivity {
            activity_id: "a".into(),
            pipeline: EmailPipelineName::ParcelTracker,
            email_id: "e".into(),
            subject: "Your order shipped".into(),
            from: "UPS <ups@ups.com>".into(),
            received_at: 0,
            processed_at: 0,
            outcome: EmailActivityOutcome::Processed,
            detail: None,
            admit_reason: None,
            admit_tier: None,
            cost_cents: None,
            items: vec!["1Z999 (UPS)".into()],
        };
        assert!(matches_query(&activity, "SHIPPED"));
        assert!(matches_query(&activity, "ups.com"));
        assert!(matches_query(&activity, "1z999"));
        assert!(!matches_query(&activity, "fedex"));
    }
}
