//! The outbox: every write the app makes, kept in order until the server has
//! it. The server applies each client id once, so a resend is safe.

use std::collections::BTreeSet;

use omni_api::fond::{
    ClientOp, ClientOpKind, CookEvent, CookSession, KitchenRequest, OpsRequest, RecipeRecord,
    RecipeState, RequestStatus, StartCookRequest,
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "job", rename_all = "camelCase")]
pub enum Job {
    Op {
        op: ClientOp,
    },
    StartCook {
        request: StartCookRequest,
    },
    CookEvent {
        session: String,
        event: CookEvent,
    },
    /// `blob` names the stored image bytes.
    CookPhoto {
        session: String,
        blob: String,
    },
    InboxPhoto {
        item: String,
        index: u32,
        count: u32,
        title: Option<String>,
        blob: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    /// Increases with every entry; the send order.
    pub key: u64,
    pub job: Job,
}

/// What to send next: the head of the queue with the entries directly behind
/// it that travel in the same request.
#[derive(Debug, Clone, PartialEq)]
pub enum Batch {
    Ops {
        keys: Vec<u64>,
        request: OpsRequest,
    },
    Start {
        key: u64,
        request: StartCookRequest,
    },
    Events {
        keys: Vec<u64>,
        session: String,
        events: Vec<CookEvent>,
    },
    CookPhoto {
        key: u64,
        session: String,
        blob: String,
    },
    InboxPhoto {
        key: u64,
        item: String,
        index: u32,
        count: u32,
        title: Option<String>,
        blob: String,
    },
}

const BATCH_MAX: usize = 50;

pub fn next_batch(entries: &[Entry]) -> Option<Batch> {
    let head = entries.first()?;
    Some(match &head.job {
        Job::Op { .. } => {
            let mut keys = Vec::new();
            let mut ops = Vec::new();
            for entry in entries.iter().take(BATCH_MAX) {
                let Job::Op { op } = &entry.job else { break };
                keys.push(entry.key);
                ops.push(op.clone());
            }
            Batch::Ops {
                keys,
                request: OpsRequest { ops },
            }
        }
        Job::StartCook { request } => Batch::Start {
            key: head.key,
            request: request.clone(),
        },
        Job::CookEvent { session, .. } => {
            let mut keys = Vec::new();
            let mut events = Vec::new();
            for entry in entries.iter().take(BATCH_MAX) {
                match &entry.job {
                    Job::CookEvent {
                        session: other,
                        event,
                    } if other == session => {
                        keys.push(entry.key);
                        events.push(event.clone());
                    }
                    _ => break,
                }
            }
            Batch::Events {
                keys,
                session: session.clone(),
                events,
            }
        }
        Job::CookPhoto { session, blob } => Batch::CookPhoto {
            key: head.key,
            session: session.clone(),
            blob: blob.clone(),
        },
        Job::InboxPhoto {
            item,
            index,
            count,
            title,
            blob,
        } => Batch::InboxPhoto {
            key: head.key,
            item: item.clone(),
            index: *index,
            count: *count,
            title: title.clone(),
            blob: blob.clone(),
        },
    })
}

/// What a response means for the batch that was sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Sent,
    /// The server answered and will never accept it; keeping it would block
    /// everything behind it.
    Drop,
    Retry,
}

/// `status` is `None` when no answer arrived.
pub fn classify(status: Option<u16>) -> Outcome {
    match status {
        Some(200..=299) => Outcome::Sent,
        Some(408 | 425 | 429) | None => Outcome::Retry,
        Some(400..=499) => Outcome::Drop,
        Some(_) => Outcome::Retry,
    }
}

/// Milliseconds to wait before attempt `failures + 1`: 1 s doubling to 60 s.
pub fn backoff_ms(failures: u32) -> u32 {
    1000u32.saturating_mul(1 << failures.min(6)).min(60_000)
}

fn notes_of<'a>(entry: &'a Entry, recipe: &str) -> Option<(&'a str, u32)> {
    match &entry.job {
        Job::Op {
            op:
                ClientOp {
                    kind:
                        ClientOpKind::Notes {
                            recipe_id,
                            text,
                            base_rev,
                        },
                    ..
                },
        } if recipe_id == recipe => Some((text, *base_rev)),
        _ => None,
    }
}

/// Queues a notes edit. An unsent edit of the same recipe at the tail of the
/// queue is replaced, so typing produces one request and keeps the first
/// edit's `base_rev`. `in_flight` entries are already on the wire and stay.
pub fn enqueue_notes(entries: &mut Vec<Entry>, in_flight: &[u64], key: u64, op: ClientOp) {
    let ClientOpKind::Notes { recipe_id, .. } = &op.kind else {
        entries.push(Entry {
            key,
            job: Job::Op { op },
        });
        return;
    };
    if let Some(last) = entries.last_mut()
        && !in_flight.contains(&last.key)
        && notes_of(last, recipe_id).is_some()
        && let Job::Op { op: queued } = &mut last.job
        && let (
            ClientOpKind::Notes {
                text: queued_text, ..
            },
            ClientOpKind::Notes { text, .. },
        ) = (&mut queued.kind, &op.kind)
    {
        queued_text.clone_from(text);
        queued.at = op.at;
        return;
    }
    entries.push(Entry {
        key,
        job: Job::Op { op },
    });
}

/// After the server stored `sent_text` as revision `stored_rev` unchanged,
/// later queued edits of the recipe build on it. When the server merged
/// instead (the stored text differs), they keep their stale `base_rev` so the
/// server keeps both texts again rather than losing the merge.
pub fn rebase_notes(entries: &mut [Entry], recipe: &str, sent_text: &str, stored: &RecipeRecord) {
    if stored.notes != sent_text {
        return;
    }
    for entry in entries {
        if let Job::Op { op } = &mut entry.job
            && let ClientOpKind::Notes {
                recipe_id,
                base_rev,
                ..
            } = &mut op.kind
            && recipe_id == recipe
        {
            *base_rev = stored.notes_rev;
        }
    }
}

/// The newest queued notes text for `recipe`.
pub fn pending_notes<'a>(entries: &'a [Entry], recipe: &str) -> Option<&'a str> {
    entries
        .iter()
        .rev()
        .find_map(|entry| notes_of(entry, recipe))
        .map(|(text, _)| text)
}

/// The newest queued state for `recipe`.
pub fn pending_state(entries: &[Entry], recipe: &str) -> Option<RecipeState> {
    entries.iter().rev().find_map(|entry| match &entry.job {
        Job::Op {
            op:
                ClientOp {
                    kind: ClientOpKind::State { recipe_id, state },
                    ..
                },
        } if recipe_id == recipe => Some(*state),
        _ => None,
    })
}

/// A server copy of a recipe with the app's unsent edits on top, so a sync
/// that lands before the outbox drains does not undo them on screen.
pub fn overlay_pending(mut record: RecipeRecord, entries: &[Entry]) -> RecipeRecord {
    for entry in entries {
        if let Job::Op {
            op:
                ClientOp {
                    kind:
                        ClientOpKind::Amount {
                            recipe_id,
                            ingredient_id,
                            qty,
                            unit,
                            ..
                        },
                    ..
                },
        } = &entry.job
            && *recipe_id == record.id
            && let Some(changed) = with_amount(&record, ingredient_id, *qty, unit.as_deref())
        {
            record = changed;
        }
    }
    if let Some(text) = pending_notes(entries, &record.id) {
        record.notes = text.to_owned();
    }
    if let Some(state) = pending_state(entries, &record.id) {
        record.state = state;
    }
    record
}

/// `record` with one ingredient's amount changed, as the server will write
/// it: a new revision of the document. `None` when the change does not apply
/// or changes nothing.
pub fn with_amount(
    record: &RecipeRecord,
    ingredient: &str,
    qty: f64,
    unit: Option<&str>,
) -> Option<RecipeRecord> {
    let doc = omni_recipe_format::upgrade(record.format, record.doc.clone()).ok()?;
    let ops = omni_recipe_format::amount_ops(&doc, ingredient, qty, unit).ok()?;
    if ops.is_empty() {
        return None;
    }
    let changed = omni_recipe_format::apply_ops(&doc, &ops).ok()?;
    if !omni_recipe_format::validate(&changed).is_valid() {
        return None;
    }
    let mut record = record.clone();
    record.doc = serde_json::to_value(&changed).ok()?;
    record.format = omni_recipe_format::FORMAT;
    record.revision += 1;
    Some(record)
}

/// Client event ids of cook events not yet sent.
pub fn pending_event_ids(entries: &[Entry]) -> BTreeSet<String> {
    entries
        .iter()
        .filter_map(|entry| match &entry.job {
            Job::CookEvent { event, .. } => Some(event.client_event_id.clone()),
            _ => None,
        })
        .collect()
}

/// Local events of `session` that were never sent and are not queued: the
/// app stopped between storing the session and storing the outbox entry.
/// They are queued again at launch, so a tap is never silently undone.
pub fn unqueued_events(entries: &[Entry], session: &CookSession) -> Vec<CookEvent> {
    let pending = pending_event_ids(entries);
    session
        .events
        .iter()
        .filter(|event| event.seq == 0 && !pending.contains(&event.client_event_id))
        .cloned()
        .collect()
}

/// Whether anything queued still belongs to `session`, which keeps a session
/// the server does not list (not started there yet) alive locally.
pub fn session_pending(entries: &[Entry], session: &str) -> bool {
    entries.iter().any(|entry| match &entry.job {
        Job::StartCook { request } => request.id == session,
        Job::CookEvent { session: other, .. } | Job::CookPhoto { session: other, .. } => {
            other == session
        }
        _ => false,
    })
}

/// Removes everything queued for `session` and returns what was removed.
/// The server answers events of a session it has never seen with 425 (they
/// may simply be ahead of their start), so once a start is refused for good
/// its events would otherwise be retried forever and block the queue.
pub fn drop_session(entries: &mut Vec<Entry>, session: &str) -> Vec<Entry> {
    let (gone, kept) = std::mem::take(entries)
        .into_iter()
        .partition(|entry| session_pending(std::slice::from_ref(entry), session));
    *entries = kept;
    gone
}

/// Requests written here and not yet sent, newest first, as the Requests
/// page lists them until the server has them.
pub fn pending_requests(entries: &[Entry]) -> Vec<KitchenRequest> {
    entries
        .iter()
        .rev()
        .filter_map(|entry| match &entry.job {
            Job::Op {
                op:
                    ClientOp {
                        at,
                        kind:
                            ClientOpKind::Request {
                                request_id,
                                text,
                                origin,
                                recipe_id,
                                ..
                            },
                        ..
                    },
            } => Some(KitchenRequest {
                id: request_id.clone(),
                text: text.clone(),
                at: *at,
                updated_at: *at,
                origin: *origin,
                status: RequestStatus::Queued,
                recipe_id: recipe_id.clone(),
                recipe_title: None,
                note: None,
                result_recipe_id: None,
                result_revision: None,
            }),
            _ => None,
        })
        .collect()
}

/// Photos of inbox `item` still to upload.
pub fn inbox_remaining(entries: &[Entry], item: &str) -> u32 {
    let count = entries
        .iter()
        .filter(|entry| matches!(&entry.job, Job::InboxPhoto { item: other, .. } if other == item))
        .count();
    u32::try_from(count).unwrap_or(u32::MAX)
}

/// The inbox items with photos still queued: `(item, count, title)`, oldest
/// first.
pub fn inbox_items(entries: &[Entry]) -> Vec<(String, u32, Option<String>)> {
    let mut items: Vec<(String, u32, Option<String>)> = Vec::new();
    for entry in entries {
        if let Job::InboxPhoto {
            item, count, title, ..
        } = &entry.job
            && !items.iter().any(|(known, _, _)| known == item)
        {
            items.push((item.clone(), *count, title.clone()));
        }
    }
    items
}

#[cfg(test)]
mod tests {
    use omni_api::fond::{CookEventKind, ImageInfo};

    use super::*;
    use crate::logic::session::app_event;

    fn notes(id: &str, recipe: &str, text: &str, base_rev: u32) -> ClientOp {
        ClientOp {
            id: id.into(),
            at: 1,
            kind: ClientOpKind::Notes {
                recipe_id: recipe.into(),
                text: text.into(),
                base_rev,
            },
        }
    }

    #[test]
    fn unsent_requests_are_listed_newest_first_and_travel_with_other_ops() {
        use omni_api::fond::RequestOrigin;
        let ask = |id: &str, request: &str, at: i64, recipe: Option<&str>| ClientOp {
            id: id.into(),
            at,
            kind: ClientOpKind::Request {
                request_id: request.into(),
                text: format!("text of {request}"),
                origin: if recipe.is_some() {
                    RequestOrigin::Recipe
                } else {
                    RequestOrigin::Library
                },
                recipe_id: recipe.map(str::to_owned),
                revision: recipe.map(|_| 3),
                scale: None,
                step: None,
            },
        };
        let entries = vec![
            Entry {
                key: 1,
                job: Job::Op {
                    op: ask("op_a", "req_aaaaaaaa", 10, Some("rcp_a")),
                },
            },
            Entry {
                key: 2,
                job: Job::Op {
                    op: notes("op_n", "rcp_a", "salt", 0),
                },
            },
            Entry {
                key: 3,
                job: Job::Op {
                    op: ask("op_b", "req_bbbbbbbb", 20, None),
                },
            },
        ];
        let pending = pending_requests(&entries);
        let ids: Vec<&str> = pending.iter().map(|request| request.id.as_str()).collect();
        assert_eq!(ids, ["req_bbbbbbbb", "req_aaaaaaaa"]);
        assert_eq!(pending[1].recipe_id.as_deref(), Some("rcp_a"));
        assert_eq!(pending[1].status, RequestStatus::Queued);
        // One request to the server carries the requests and the note between them.
        let Some(Batch::Ops { keys, request }) = next_batch(&entries) else {
            panic!("expected an ops batch");
        };
        assert_eq!(keys, [1, 2, 3]);
        let sent = serde_json::to_value(&request).unwrap();
        assert_eq!(sent["ops"][0]["type"], "request");
        assert_eq!(sent["ops"][0]["requestId"], "req_aaaaaaaa");
        assert_eq!(sent["ops"][0]["revision"], 3);
        assert!(sent["ops"][2].get("recipeId").is_none());
        // A request leaves a recipe on screen as it was.
        assert!(pending_notes(&entries[..1], "rcp_a").is_none());
        assert!(pending_state(&entries, "rcp_a").is_none());
    }

    fn state(id: &str, recipe: &str, state: RecipeState) -> ClientOp {
        ClientOp {
            id: id.into(),
            at: 1,
            kind: ClientOpKind::State {
                recipe_id: recipe.into(),
                state,
            },
        }
    }

    fn op(key: u64, op: ClientOp) -> Entry {
        Entry {
            key,
            job: Job::Op { op },
        }
    }

    fn event(key: u64, session: &str, id: &str) -> Entry {
        Entry {
            key,
            job: Job::CookEvent {
                session: session.into(),
                event: app_event(
                    id.into(),
                    1,
                    CookEventKind::Check {
                        target: "sear".into(),
                    },
                ),
            },
        }
    }

    fn start(key: u64, session: &str) -> Entry {
        Entry {
            key,
            job: Job::StartCook {
                request: StartCookRequest {
                    id: session.into(),
                    recipe_id: "r1".into(),
                    revision: 1,
                    scale: 1.0,
                    at: 1,
                },
            },
        }
    }

    fn record(notes: &str, notes_rev: u32) -> RecipeRecord {
        RecipeRecord {
            id: "r1".into(),
            revision: 1,
            format: 1,
            state: RecipeState::Active,
            doc: serde_json::Value::Null,
            tags: vec![],
            notes: notes.into(),
            notes_rev,
            images: Vec::<ImageInfo>::new(),
            cover: None,
            source: None,
            active_min: 0,
            total_min: 0,
            cook_count: 0,
            last_cooked_at: None,
            created_at: 0,
            updated_at: 0,
            seq: 1,
            adjustments: Vec::new(),
        }
    }

    #[test]
    fn batches_follow_queue_order_and_group_only_neighbours() {
        let queue = vec![
            op(1, state("a", "r1", RecipeState::Archived)),
            op(2, notes("b", "r1", "x", 0)),
            start(3, "cook_a"),
            event(4, "cook_a", "e1"),
            event(5, "cook_a", "e2"),
            event(6, "cook_b", "e3"),
            op(7, notes("c", "r2", "y", 0)),
        ];
        let Some(Batch::Ops { keys, request }) = next_batch(&queue) else {
            panic!("expected ops");
        };
        assert_eq!(keys, [1, 2]);
        assert_eq!(request.ops.len(), 2);
        assert!(matches!(
            next_batch(&queue[2..]),
            Some(Batch::Start { key: 3, .. })
        ));
        let Some(Batch::Events {
            keys,
            session,
            events,
        }) = next_batch(&queue[3..])
        else {
            panic!("expected events");
        };
        assert_eq!(
            (keys, session.as_str(), events.len()),
            (vec![4, 5], "cook_a", 2)
        );
        let Some(Batch::Events { keys, .. }) = next_batch(&queue[5..]) else {
            panic!("expected events");
        };
        assert_eq!(keys, [6]);
        assert_eq!(next_batch(&[]), None);
    }

    #[test]
    fn photos_travel_alone() {
        let queue = vec![
            Entry {
                key: 1,
                job: Job::InboxPhoto {
                    item: "inb_1".into(),
                    index: 0,
                    count: 2,
                    title: None,
                    blob: "b1".into(),
                },
            },
            Entry {
                key: 2,
                job: Job::InboxPhoto {
                    item: "inb_1".into(),
                    index: 1,
                    count: 2,
                    title: None,
                    blob: "b2".into(),
                },
            },
        ];
        assert!(matches!(
            next_batch(&queue),
            Some(Batch::InboxPhoto {
                key: 1,
                index: 0,
                ..
            })
        ));
        assert_eq!(inbox_remaining(&queue, "inb_1"), 2);
        assert_eq!(inbox_remaining(&queue[1..], "inb_1"), 1);
        assert_eq!(inbox_items(&queue), [("inb_1".to_owned(), 2, None)]);
    }

    #[test]
    fn answers_decide_between_sent_drop_and_retry() {
        assert_eq!(classify(Some(200)), Outcome::Sent);
        assert_eq!(classify(Some(204)), Outcome::Sent);
        assert_eq!(classify(Some(400)), Outcome::Drop);
        assert_eq!(classify(Some(404)), Outcome::Drop);
        assert_eq!(classify(Some(413)), Outcome::Drop);
        assert_eq!(classify(Some(408)), Outcome::Retry);
        assert_eq!(classify(Some(429)), Outcome::Retry);
        assert_eq!(classify(Some(502)), Outcome::Retry);
        assert_eq!(classify(None), Outcome::Retry);
    }

    #[test]
    fn backoff_doubles_to_a_minute() {
        assert_eq!(backoff_ms(0), 1000);
        assert_eq!(backoff_ms(1), 2000);
        assert_eq!(backoff_ms(5), 32_000);
        assert_eq!(backoff_ms(6), 60_000);
        assert_eq!(backoff_ms(40), 60_000);
    }

    #[test]
    fn typing_replaces_the_unsent_notes_edit_but_not_one_on_the_wire() {
        let mut queue = vec![op(1, notes("a", "r1", "h", 4))];
        enqueue_notes(&mut queue, &[], 2, notes("b", "r1", "he", 4));
        assert_eq!(queue.len(), 1);
        assert_eq!(pending_notes(&queue, "r1"), Some("he"));
        assert!(matches!(&queue[0].job, Job::Op { op } if op.id == "a"));
        // Entry 1 is being sent: the next edit queues behind it.
        enqueue_notes(&mut queue, &[1], 3, notes("c", "r1", "hel", 4));
        assert_eq!(queue.len(), 2);
        // Another recipe's edit never merges.
        enqueue_notes(&mut queue, &[], 4, notes("d", "r2", "x", 0));
        assert_eq!(queue.len(), 3);
        // An edit separated by another job is not at the tail.
        enqueue_notes(&mut queue, &[], 5, notes("e", "r1", "hell", 4));
        assert_eq!(queue.len(), 4);
        assert_eq!(pending_notes(&queue, "r1"), Some("hell"));
    }

    #[test]
    fn later_edits_build_on_a_clean_save_and_stay_stale_after_a_merge() {
        let mut queue = vec![op(2, notes("b", "r1", "hello", 4))];
        rebase_notes(&mut queue, "r1", "hel", &record("hel", 5));
        assert!(matches!(
            &queue[0].job,
            Job::Op {
                op: ClientOp {
                    kind: ClientOpKind::Notes { base_rev: 5, .. },
                    ..
                }
            }
        ));
        rebase_notes(&mut queue, "r1", "hel", &record("agent text\n\nhel", 6));
        assert!(matches!(
            &queue[0].job,
            Job::Op {
                op: ClientOp {
                    kind: ClientOpKind::Notes { base_rev: 5, .. },
                    ..
                }
            }
        ));
    }

    #[test]
    fn unsent_edits_show_over_a_synced_record() {
        let queue = vec![
            op(1, state("a", "r1", RecipeState::Trashed)),
            op(2, notes("b", "r1", "mine", 0)),
            op(3, state("c", "r1", RecipeState::Active)),
        ];
        let shown = overlay_pending(record("server", 3), &queue);
        assert_eq!(shown.notes, "mine");
        assert_eq!(shown.state, RecipeState::Active);
        assert_eq!(shown.notes_rev, 3);
        assert_eq!(pending_state(&queue, "r2"), None);
        assert_eq!(overlay_pending(record("server", 3), &[]).notes, "server");
    }

    fn amount(id: &str, ingredient: &str, qty: f64, unit: Option<&str>, base: u32) -> ClientOp {
        ClientOp {
            id: id.into(),
            at: 1,
            kind: ClientOpKind::Amount {
                recipe_id: "r1".into(),
                ingredient_id: ingredient.into(),
                qty,
                unit: unit.map(str::to_owned),
                base_revision: base,
            },
        }
    }

    #[test]
    fn an_unsent_amount_is_a_new_revision_over_a_synced_record() {
        let mut server = record("", 0);
        server.doc = serde_json::from_str(include_str!(
            "../../../omni/src/preview_recipes/mustard-vinaigrette.json"
        ))
        .unwrap();
        let qty = |record: &RecipeRecord, id: &str| {
            record.doc["components"][0]["ingredients"]
                .as_array()
                .unwrap()
                .iter()
                .find(|ingredient| ingredient["id"] == id)
                .unwrap()
                .clone()
        };
        // The edit and its undo are both queued: the record shows the last.
        let queue = vec![
            op(1, amount("a", "salt", 1.0, Some("tsp"), 1)),
            op(2, amount("b", "oil", 60.0, Some("ml"), 2)),
        ];
        let shown = overlay_pending(server.clone(), &queue);
        assert_eq!(shown.revision, 3);
        assert_eq!(qty(&shown, "salt")["qty"], 1);
        assert_eq!(qty(&shown, "oil")["qty"], 60);
        assert_eq!(
            qty(&shown, "oil")["alt"]["qty"],
            0.25,
            "the other measure follows"
        );
        // The wire form is what the server reads.
        assert_eq!(
            serde_json::to_value(amount("a", "salt", 1.0, Some("tsp"), 1)).unwrap(),
            serde_json::json!({ "id": "a", "at": 1, "type": "amount", "recipeId": "r1",
                "ingredientId": "salt", "qty": 1.0, "unit": "tsp", "baseRevision": 1 })
        );
        // What cannot apply changes nothing: no amount, no such ingredient,
        // the same amount, a unit that is not offered.
        for refused in [
            amount("c", "nothing", 1.0, None, 1),
            amount("d", "salt", 0.5, Some("tsp"), 1),
            amount("e", "salt", 1.0, Some("glug"), 1),
            amount("f", "salt", 0.0, Some("tsp"), 1),
        ] {
            assert_eq!(overlay_pending(server.clone(), &[op(1, refused)]), server);
        }
        assert!(with_amount(&server, "salt", 1.0, Some("tsp")).is_some());
    }

    #[test]
    fn a_session_stays_while_anything_of_it_is_queued() {
        let queue = vec![start(1, "cook_a"), event(2, "cook_b", "e9")];
        assert!(session_pending(&queue, "cook_a"));
        assert!(session_pending(&queue, "cook_b"));
        assert!(!session_pending(&queue, "cook_c"));
        assert_eq!(pending_event_ids(&queue), BTreeSet::from(["e9".to_owned()]));
    }

    #[test]
    fn a_refused_session_takes_its_queued_jobs_with_it() {
        let mut queue = vec![
            start(1, "cook_a"),
            event(2, "cook_a", "e1"),
            op(3, notes("a", "r1", "x", 0)),
            event(4, "cook_b", "e2"),
            Entry {
                key: 5,
                job: Job::CookPhoto {
                    session: "cook_a".into(),
                    blob: "b1".into(),
                },
            },
        ];
        let gone = drop_session(&mut queue, "cook_a");
        assert_eq!(gone.iter().map(|e| e.key).collect::<Vec<_>>(), [1, 2, 5]);
        assert_eq!(queue.iter().map(|e| e.key).collect::<Vec<_>>(), [3, 4]);
    }

    #[test]
    fn stored_events_without_an_outbox_entry_are_found() {
        let mut sent = app_event("e0".into(), 1, CookEventKind::Check { target: "a".into() });
        sent.seq = 4;
        let session = CookSession {
            id: "cook_a".into(),
            recipe_id: "r1".into(),
            revision: 1,
            scale: 1.0,
            state: omni_api::fond::CookState::Active,
            started_at: 0,
            finished_at: None,
            last_event_at: 0,
            events: vec![
                sent,
                app_event(
                    "e1".into(),
                    1,
                    CookEventKind::Check {
                        target: "sear".into(),
                    },
                ),
                app_event("e2".into(), 1, CookEventKind::Check { target: "b".into() }),
            ],
            doc: None,
        };
        let queue = vec![event(1, "cook_a", "e1")];
        let lost = unqueued_events(&queue, &session);
        assert_eq!(
            lost.iter()
                .map(|e| e.client_event_id.as_str())
                .collect::<Vec<_>>(),
            ["e2"]
        );
    }

    #[test]
    fn entries_survive_storage() {
        let entry = event(7, "cook_a", "e1");
        let text = serde_json::to_string(&entry).unwrap();
        assert_eq!(serde_json::from_str::<Entry>(&text).unwrap(), entry);
        let entry = op(8, notes("a", "r1", "x", 2));
        let text = serde_json::to_string(&entry).unwrap();
        assert_eq!(serde_json::from_str::<Entry>(&text).unwrap(), entry);
    }
}
