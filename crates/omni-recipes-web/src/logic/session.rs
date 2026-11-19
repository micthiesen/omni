//! A cook session is an event log; everything cook mode shows is this
//! projection of it.

use std::collections::{BTreeMap, BTreeSet};

use omni_api::common::Ms;
use omni_api::fond::{CookActor, CookEvent, CookEventKind, CookSession, CookState};
use omni_recipe_format::{Index, RecipeDoc, ready_steps};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Projection {
    /// The revision the session is on after any `rebase`.
    pub revision: u32,
    /// Step ids (done), ingredient ids (gathered) and `step/ref` keys (added).
    pub checked: BTreeSet<String>,
    pub notes: Vec<String>,
    /// Running or rung timers by step id, as end times.
    pub timers: BTreeMap<String, Ms>,
    pub finished: bool,
}

/// Replays `session.events` in order; a later event wins over an earlier one.
pub fn project(session: &CookSession) -> Projection {
    let mut state = Projection {
        revision: session.revision,
        finished: session.state != CookState::Active,
        ..Projection::default()
    };
    for event in &session.events {
        match &event.kind {
            CookEventKind::Check { target } => {
                state.checked.insert(target.clone());
            }
            CookEventKind::Uncheck { target } => {
                state.checked.remove(target);
            }
            CookEventKind::Note { text } => state.notes.push(text.clone()),
            CookEventKind::Timer { step, ends_at } => {
                state.timers.insert(step.clone(), *ends_at);
            }
            CookEventKind::CancelTimer { step } => {
                state.timers.remove(step);
            }
            CookEventKind::Rebase { revision } => state.revision = *revision,
            CookEventKind::Finish { .. } => state.finished = true,
            CookEventKind::Abandon {} => {
                state.finished = true;
                state.timers.clear();
            }
            CookEventKind::Unknown => {}
        }
    }
    state
}

impl Projection {
    /// The done steps of `doc`, in written order. Checked ids the document no
    /// longer has (after a rebase) are ignored.
    pub fn done_steps(&self, doc: &RecipeDoc) -> Vec<String> {
        Index::build(doc)
            .steps()
            .filter(|(_, step)| self.checked.contains(&step.id))
            .map(|(_, step)| step.id.clone())
            .collect()
    }

    /// Whether the input `key` of `step` has gone in: checked itself, or its
    /// step is done.
    pub fn input_checked(&self, step: &str, key: &str) -> bool {
        self.checked.contains(key) || self.checked.contains(step)
    }

    /// `(done, total)` steps.
    pub fn progress(&self, doc: &RecipeDoc) -> (usize, usize) {
        let total = doc.components.iter().map(|c| c.steps.len()).sum();
        (self.done_steps(doc).len(), total)
    }

    /// The event that flips `target`.
    pub fn toggle(&self, target: &str) -> CookEventKind {
        let target = target.to_owned();
        if self.checked.contains(&target) {
            CookEventKind::Uncheck { target }
        } else {
            CookEventKind::Check { target }
        }
    }

    /// The step the Steps tab shows: `preferred` while it exists and is not
    /// done, else the first ready step, else the last step when all are done.
    pub fn current_step(&self, doc: &RecipeDoc, preferred: Option<&str>) -> Option<String> {
        let ix = Index::build(doc);
        if let Some(id) = preferred
            && ix.step(id).is_some()
            && !self.checked.contains(id)
        {
            return Some(id.to_owned());
        }
        let done = self.done_steps(doc);
        ready_steps(doc, &done)
            .into_iter()
            .next()
            .or_else(|| ix.steps().last().map(|(_, step)| step.id.clone()))
    }

    /// Where Done on `step` leads: the first ready step once `step` is done,
    /// preferring one written after it.
    pub fn next_after(&self, doc: &RecipeDoc, step: &str) -> Option<String> {
        let mut done = self.done_steps(doc);
        if !done.iter().any(|id| id == step) {
            done.push(step.to_owned());
        }
        let ready = ready_steps(doc, &done);
        let ix = Index::build(doc);
        let at = ix.step_pos(step).unwrap_or(0);
        ready
            .iter()
            .find(|id| ix.step_pos(id).is_some_and(|pos| pos > at))
            .or_else(|| ready.first())
            .cloned()
    }
}

/// An event the app is about to append; the server assigns `seq`.
pub fn app_event(client_event_id: String, at: Ms, kind: CookEventKind) -> CookEvent {
    CookEvent {
        seq: 0,
        client_event_id,
        at,
        actor: CookActor::App,
        kind,
    }
}

/// The server's copy of a session with the local events it has not seen yet
/// (`pending`: client event ids still in the outbox) appended in local order.
pub fn merge_server(
    mut server: CookSession,
    local: Option<&CookSession>,
    pending: &BTreeSet<String>,
) -> CookSession {
    let Some(local) = local else { return server };
    if server.doc.is_none() && server.revision == local.revision {
        server.doc.clone_from(&local.doc);
    }
    let known: BTreeSet<&str> = server
        .events
        .iter()
        .map(|event| event.client_event_id.as_str())
        .collect();
    let unsent: Vec<CookEvent> = local
        .events
        .iter()
        .filter(|event| {
            pending.contains(&event.client_event_id)
                && !known.contains(event.client_event_id.as_str())
        })
        .cloned()
        .collect();
    if let Some(last) = unsent.last() {
        server.last_event_at = server.last_event_at.max(last.at);
    }
    server.events.extend(unsent);
    server
}

/// Whether the session should be listed under "Cooking now".
pub fn is_open(session: &CookSession) -> bool {
    !project(session).finished
}

/// Whether an open session of `recipe` moves along when the app replaces
/// revision `replaced` of it: one that shows that revision, and the session
/// the change was made from (`from`) wherever it was.
pub fn follows(session: &CookSession, recipe: &str, replaced: u32, from: Option<&str>) -> bool {
    let state = project(session);
    session.recipe_id == recipe
        && !state.finished
        && (state.revision == replaced || from == Some(session.id.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const PICCATA: &str =
        include_str!("../../../omni-recipe-format/tests/golden/chicken-piccata.json");

    fn doc() -> RecipeDoc {
        serde_json::from_str(PICCATA).unwrap()
    }

    fn session(kinds: Vec<CookEventKind>) -> CookSession {
        CookSession {
            id: "cook_abcdefgh".into(),
            recipe_id: "rcp_1".into(),
            revision: 3,
            scale: 1.0,
            state: CookState::Active,
            started_at: 1,
            finished_at: None,
            last_event_at: 1,
            events: kinds
                .into_iter()
                .enumerate()
                .map(|(n, kind)| app_event(format!("e{n}"), n as Ms, kind))
                .collect(),
            doc: None,
        }
    }

    fn check(target: &str) -> CookEventKind {
        CookEventKind::Check {
            target: target.into(),
        }
    }

    fn uncheck(target: &str) -> CookEventKind {
        CookEventKind::Uncheck {
            target: target.into(),
        }
    }

    #[test]
    fn uncheck_reverses_check_and_later_events_win() {
        let state = project(&session(vec![
            check("dredge"),
            check("sear/oil"),
            uncheck("dredge"),
            check("dredge"),
            uncheck("sear/oil"),
            check("flour"),
        ]));
        assert!(state.checked.contains("dredge"));
        assert!(state.checked.contains("flour"));
        assert!(!state.checked.contains("sear/oil"));
        assert_eq!(state.done_steps(&doc()), ["dredge"]);
        assert_eq!(state.progress(&doc()), (1, 6));
    }

    #[test]
    fn timers_notes_and_rebase_follow_the_log() {
        let state = project(&session(vec![
            CookEventKind::Timer {
                step: "simmer".into(),
                ends_at: 500,
            },
            CookEventKind::Timer {
                step: "simmer".into(),
                ends_at: 900,
            },
            CookEventKind::Timer {
                step: "sear".into(),
                ends_at: 100,
            },
            CookEventKind::CancelTimer {
                step: "sear".into(),
            },
            CookEventKind::Rebase { revision: 5 },
            CookEventKind::Note {
                text: "smells right".into(),
            },
        ]));
        assert_eq!(state.timers.get("simmer"), Some(&900));
        assert!(!state.timers.contains_key("sear"));
        assert_eq!(state.revision, 5);
        assert_eq!(state.notes, ["smells right"]);
        assert!(!state.finished);
    }

    /// A phone that cooked before overrides were retired still holds their
    /// events, in its stored sessions and in its outbox.
    #[test]
    fn retired_adjustment_events_still_decode_and_change_nothing() {
        let stored = serde_json::json!({
            "id": "cook_abcdefgh", "recipeId": "rcp_1", "revision": 3, "scale": 1.0,
            "state": "active", "startedAt": 1, "finishedAt": null, "lastEventAt": 1,
            "events": [
                { "seq": 1, "clientEventId": "e0", "at": 1, "actor": "app",
                  "type": "adjust", "target": "sear", "text": "Two batches." },
                { "seq": 2, "clientEventId": "e1", "at": 2, "actor": "app",
                  "type": "check", "target": "dredge" },
                { "seq": 0, "clientEventId": "e2", "at": 3, "actor": "app",
                  "type": "unadjust", "target": "sear" },
                { "seq": 0, "clientEventId": "e3", "at": 4, "actor": "app",
                  "type": "dismissAdjustment", "id": "adj_1" }
            ]
        });
        let session: CookSession = serde_json::from_value(stored).unwrap();
        assert_eq!(session.events.len(), 4);
        assert_eq!(session.events[0].kind, CookEventKind::Unknown);
        assert_eq!(session.events[3].kind, CookEventKind::Unknown);
        let state = project(&session);
        assert_eq!(state.checked, BTreeSet::from(["dredge".to_owned()]));
        assert_eq!(state.revision, 3);
        assert!(!state.finished);
    }

    #[test]
    fn a_changed_recipe_takes_along_the_sessions_showing_it() {
        let on_three = session(vec![check("dredge")]);
        assert!(follows(&on_three, "rcp_1", 3, None));
        assert!(!follows(&on_three, "rcp_2", 3, None));
        // A session left on an older revision stays there, unless the change
        // was made from it.
        assert!(!follows(&on_three, "rcp_1", 4, None));
        assert!(follows(&on_three, "rcp_1", 4, Some("cook_abcdefgh")));
        let rebased = session(vec![CookEventKind::Rebase { revision: 4 }]);
        assert!(follows(&rebased, "rcp_1", 4, None));
        let finished = session(vec![CookEventKind::Finish { note: None }]);
        assert!(!follows(&finished, "rcp_1", 3, Some("cook_abcdefgh")));
    }

    #[test]
    fn finish_closes_the_session() {
        let finished = session(vec![check("dredge"), CookEventKind::Finish { note: None }]);
        assert!(project(&finished).finished);
        assert!(!is_open(&finished));
        let mut closed = session(vec![]);
        closed.state = CookState::Abandoned;
        assert!(!is_open(&closed));
    }

    #[test]
    fn a_done_step_counts_its_inputs_as_added() {
        let state = project(&session(vec![check("dredge"), check("sear/oil")]));
        assert!(state.input_checked("dredge", "dredge/flour"));
        assert!(state.input_checked("sear", "sear/oil"));
        assert!(!state.input_checked("sear", "sear/butter"));
    }

    #[test]
    fn the_current_step_is_the_first_ready_one_unless_one_is_chosen() {
        let doc = doc();
        let fresh = project(&session(vec![]));
        assert_eq!(fresh.current_step(&doc, None).as_deref(), Some("dredge"));
        assert_eq!(
            fresh.current_step(&doc, Some("simmer")).as_deref(),
            Some("simmer")
        );
        assert_eq!(
            fresh.current_step(&doc, Some("gone")).as_deref(),
            Some("dredge")
        );
        let one = project(&session(vec![check("dredge")]));
        assert_eq!(
            one.current_step(&doc, Some("dredge")).as_deref(),
            Some("sear")
        );
        let all = project(&session(
            ["dredge", "sear", "deglaze", "simmer", "mount", "serve"]
                .into_iter()
                .map(check)
                .collect(),
        ));
        assert_eq!(all.current_step(&doc, None).as_deref(), Some("serve"));
    }

    #[test]
    fn done_leads_to_the_next_ready_step() {
        let doc = doc();
        let fresh = project(&session(vec![]));
        assert_eq!(fresh.next_after(&doc, "dredge").as_deref(), Some("sear"));
        // Nothing after the last step is ready, so Done offers what is left.
        let skipped = project(&session(vec![check("sear"), check("deglaze")]));
        assert_eq!(skipped.next_after(&doc, "simmer").as_deref(), Some("mount"));
        let last = project(&session(
            ["dredge", "sear", "deglaze", "simmer", "mount"]
                .into_iter()
                .map(check)
                .collect(),
        ));
        assert_eq!(last.next_after(&doc, "serve"), None);
    }

    #[test]
    fn toggle_flips_the_target() {
        let state = project(&session(vec![check("oil")]));
        assert_eq!(state.toggle("oil"), uncheck("oil"));
        assert_eq!(state.toggle("butter"), check("butter"));
    }

    #[test]
    fn merging_keeps_only_unsent_local_events() {
        let mut local = session(vec![check("dredge"), check("sear"), check("deglaze")]);
        local.events[2].at = 99;
        let mut server = session(vec![check("dredge")]);
        server.events[0].seq = 1;
        server.events.push(CookEvent {
            seq: 2,
            client_event_id: "agent1".into(),
            at: 5,
            actor: CookActor::Agent,
            kind: CookEventKind::Note {
                text: "lower heat".into(),
            },
        });
        // e1 was dropped by the server and is no longer pending; e2 is unsent.
        let pending = BTreeSet::from(["e2".to_owned()]);
        let merged = merge_server(server, Some(&local), &pending);
        let ids: Vec<&str> = merged
            .events
            .iter()
            .map(|event| event.client_event_id.as_str())
            .collect();
        assert_eq!(ids, ["e0", "agent1", "e2"]);
        assert_eq!(merged.last_event_at, 99);
        let state = project(&merged);
        assert!(state.checked.contains("deglaze"));
        assert!(!state.checked.contains("sear"));
        assert_eq!(state.notes, ["lower heat"]);
    }
}
