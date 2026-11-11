//! Tracked calendar event persistence, plus store round trips
//! for the tracked-event operations.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::cell::Cell;

use jiff::tz::TimeZone;
use omni_calendar::extraction::schema::{EventRecurrence, RecurrenceFrequency};
use omni_calendar::persistence::{
    self, CreateBlock, CreatedCalendarEvent, EventFields, EventHandles, EventStatus,
    compute_calendar_event_uid, compute_event_hash, create_block, find_event, has_event_changed,
    normalize_title, pick_by_start_date, resolve_event_reference, resolve_explicit_event_reference,
    select_recent_events,
};
use omni_store::cbor::{self, Extra, JsValue};
use omni_store::{DocOps as _, Entity as _};
use omni_testkit::{TEST_EPOCH_MS, TestStore, test_clock};

fn rec() -> CreatedCalendarEvent {
    CreatedCalendarEvent {
        event_hash: "h".to_owned(),
        email_id: "e".to_owned(),
        calendar_event_id: "cal".to_owned(),
        title: "🦷 Dentist Appointment".to_owned(),
        start_date: "2026-06-17".to_owned(),
        start_time: None,
        end_date: None,
        end_time: None,
        all_day: Some(false),
        location: None,
        time_zone: None,
        description: None,
        duration: None,
        reminder_minutes: None,
        recurrence: None,
        created_at: 0,
        status: None,
        deleted_at: None,
        earlier_email_ids: Vec::new(),
        extra: Extra::default(),
    }
}

fn with(hash: &str, title: &str, start_date: &str) -> CreatedCalendarEvent {
    CreatedCalendarEvent {
        event_hash: hash.to_owned(),
        title: title.to_owned(),
        start_date: start_date.to_owned(),
        ..rec()
    }
}

fn recurrence(frequency: RecurrenceFrequency, until: &str) -> Option<EventRecurrence> {
    Some(EventRecurrence {
        frequency,
        until: until.to_owned(),
    })
}

// describe("normalizeTitle")

#[test]
fn collapses_emoji_arrow_style_casing_and_whitespace_drift_to_one_form() {
    let variants = [
        "✈️ Flight YYZ → YVR",
        "Flight YYZ -> YVR",
        "flight   yyz  yvr",
        "✈️  FLIGHT YYZ → YVR!",
    ];
    let normalized: Vec<String> = variants.iter().map(|v| normalize_title(v)).collect();
    assert!(normalized.iter().all(|n| n == &normalized[0]));
    assert_eq!(normalized[0], "flight yyz yvr");
}

#[test]
fn keeps_genuinely_different_titles_distinct() {
    assert_ne!(
        normalize_title("🦷 Dentist Appointment"),
        normalize_title("🛂 Passport Renewal")
    );
}

#[test]
fn does_not_collapse_same_token_titles_that_differ_only_in_order() {
    assert_ne!(
        normalize_title("✈️ Flight YYZ → YVR"),
        normalize_title("✈️ Flight YVR → YYZ")
    );
}

#[test]
fn keeps_letters_and_digits_of_every_script() {
    assert_eq!(
        normalize_title("Café Ünïcödé 東京 ٣"),
        "café ünïcödé 東京 ٣"
    );
    assert_eq!(
        normalize_title("\u{FEFF}Tab\tand\u{00A0}nbsp "),
        "tab and nbsp"
    );
}

// describe("computeEventHash")

#[test]
fn is_stable_across_title_drift_for_the_same_date_time() {
    assert_eq!(
        compute_event_hash("✈️ Flight YYZ → YVR", "2026-07-02", Some("08:00")),
        compute_event_hash("Flight YYZ -> YVR", "2026-07-02", Some("08:00"))
    );
    assert_eq!(
        compute_event_hash("Tax Payment Deadline", "2026-04-30", None),
        "tax payment deadline|2026-04-30|allday"
    );
}

#[test]
fn differs_when_the_date_or_time_differs() {
    assert_ne!(
        compute_event_hash("🦷 Dentist", "2026-06-17", Some("09:00")),
        compute_event_hash("🦷 Dentist", "2026-06-17", Some("10:00"))
    );
}

// describe("computeCalendarEventUid")

#[test]
fn gives_the_same_caldav_resource_to_every_replay_of_one_logical_event() {
    let hash = compute_event_hash("Dentist", "2026-06-17", Some("09:00"));
    let uid = compute_calendar_event_uid(&hash);
    assert_eq!(uid, compute_calendar_event_uid(&hash));
    let digest = uid
        .strip_prefix("omni-")
        .and_then(|rest| rest.strip_suffix("@omni-notify"))
        .unwrap();
    assert_eq!(digest.len(), 32);
    assert!(
        digest
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    );
    assert_ne!(compute_calendar_event_uid(&format!("{hash}|changed")), uid);
}

// describe("hasEventChanged")

fn base() -> CreatedCalendarEvent {
    CreatedCalendarEvent {
        start_time: Some("14:30".to_owned()),
        location: Some("Clinic".to_owned()),
        description: Some("Bring insurance card".to_owned()),
        reminder_minutes: Some(60.0),
        ..rec()
    }
}

#[test]
fn returns_false_when_nothing_meaningful_changed() {
    let b = base();
    assert!(!has_event_changed(&b, &EventFields::from(&b.clone())));
}

#[test]
fn treats_matching_undefined_optional_fields_as_unchanged() {
    let a = CreatedCalendarEvent {
        title: "X".to_owned(),
        all_day: Some(true),
        ..rec()
    };
    let fields = EventFields {
        title: "X",
        start_date: "2026-06-17",
        all_day: Some(true),
        ..EventFields::default()
    };
    assert!(!has_event_changed(&a, &fields));
}

#[test]
fn detects_a_change_to_each_field() {
    let b = base();
    let cases: Vec<(&str, CreatedCalendarEvent)> = vec![
        (
            "title (semantic)",
            CreatedCalendarEvent {
                title: "🦷 Dentist Checkup".to_owned(),
                ..b.clone()
            },
        ),
        (
            "startTime",
            CreatedCalendarEvent {
                start_time: Some("15:00".to_owned()),
                ..b.clone()
            },
        ),
        (
            "location",
            CreatedCalendarEvent {
                location: Some("New Clinic".to_owned()),
                ..b.clone()
            },
        ),
        (
            "description",
            CreatedCalendarEvent {
                description: Some("Changed".to_owned()),
                ..b.clone()
            },
        ),
        (
            "duration",
            CreatedCalendarEvent {
                duration: Some("PT2H".to_owned()),
                ..b.clone()
            },
        ),
        (
            "reminderMinutes",
            CreatedCalendarEvent {
                reminder_minutes: Some(1440.0),
                ..b.clone()
            },
        ),
    ];
    for (label, patched) in cases {
        assert!(
            has_event_changed(&b, &EventFields::from(&patched)),
            "detects a change to {label}"
        );
    }
}

#[test]
fn ignores_cosmetic_title_drift_emoji_punctuation() {
    let b = base();
    let drift = CreatedCalendarEvent {
        title: "Dentist Appointment!".to_owned(),
        ..b.clone()
    };
    assert!(!has_event_changed(&b, &EventFields::from(&drift)));
}

#[test]
fn detects_added_removed_and_modified_recurrence() {
    let b = base();
    let recurring = CreatedCalendarEvent {
        recurrence: recurrence(RecurrenceFrequency::Daily, "2026-07-13"),
        ..b.clone()
    };
    assert!(has_event_changed(&b, &EventFields::from(&recurring)));
    assert!(has_event_changed(&recurring, &EventFields::from(&b)));
    let modified = CreatedCalendarEvent {
        recurrence: recurrence(RecurrenceFrequency::Daily, "2026-07-20"),
        ..b.clone()
    };
    assert!(has_event_changed(&recurring, &EventFields::from(&modified)));
}

#[test]
fn treats_identical_recurrence_and_null_vs_undefined_as_unchanged() {
    let b = base();
    let recurring = CreatedCalendarEvent {
        recurrence: recurrence(RecurrenceFrequency::Weekly, "2026-08-01"),
        ..b.clone()
    };
    let same = recurring.clone();
    assert!(!has_event_changed(&recurring, &EventFields::from(&same)));
    // `null` and absent both decode to `None`.
    assert!(!has_event_changed(&b, &EventFields::from(&b.clone())));
}

// describe("selectRecentEvents")

/// `Date.parse("2026-04-15T12:00:00Z")`.
const NOW: i64 = 1_776_254_400_000;

fn window_events() -> Vec<CreatedCalendarEvent> {
    vec![
        with("old", "x", "2026-04-01"),
        with("recent-past", "x", "2026-04-12"),
        with("soon", "x", "2026-05-01"),
        with("september", "x", "2026-09-10"),
        with("next-year", "x", "2027-06-01"),
        CreatedCalendarEvent {
            status: Some(EventStatus::Cancelled),
            ..with("cancelled", "x", "2026-05-01")
        },
    ]
}

fn hashes(events: &[CreatedCalendarEvent]) -> Vec<&str> {
    events.iter().map(|e| e.event_hash.as_str()).collect()
}

#[test]
fn includes_far_future_events_within_a_365_day_horizon() {
    let tz = TimeZone::get("America/Vancouver").unwrap();
    let selected = select_recent_events(window_events(), 365, NOW, &tz);
    assert_eq!(hashes(&selected), ["recent-past", "soon", "september"]);
}

#[test]
fn hides_far_future_events_under_the_old_90_day_horizon_the_dup_cluster_bug() {
    let tz = TimeZone::get("America/Vancouver").unwrap();
    let selected = select_recent_events(window_events(), 90, NOW, &tz);
    assert_eq!(hashes(&selected), ["recent-past", "soon"]);
}

// describe("findEvent")

#[test]
fn matches_title_date_within_the_given_candidate_set() {
    let target = with("t", "💇 Haircut", "2026-07-20");
    let other = with("o", "💇 Haircut", "2026-08-20");
    let candidates = [target.clone(), other];
    assert_eq!(
        find_event("Haircut", "2026-07-20", &candidates),
        Some(&candidates[0])
    );
}

#[test]
fn resolves_a_lone_in_window_candidate_even_when_stale_same_title_events_exist_outside_the_set() {
    let in_window = [with("current", "💇 Haircut", "2026-07-20")];
    assert_eq!(
        find_event("Haircut", "2026-07-21", &in_window),
        Some(&in_window[0])
    );
}

#[test]
fn fails_closed_when_several_in_window_candidates_share_the_title_and_none_match_the_date() {
    let candidates = [
        with("a", "💇 Haircut", "2026-07-20"),
        with("b", "💇 Haircut", "2026-08-20"),
    ];
    assert_eq!(find_event("Haircut", "2026-09-01", &candidates), None);
}

#[test]
fn ignores_cancelled_candidates() {
    let cancelled = [CreatedCalendarEvent {
        status: Some(EventStatus::Cancelled),
        ..with("c", "💇 Haircut", "2026-07-20")
    }];
    assert_eq!(find_event("Haircut", "2026-07-20", &cancelled), None);
}

// describe("resolveExplicitEventReference")

fn handles(entries: &[(&str, CreatedCalendarEvent)]) -> EventHandles {
    entries
        .iter()
        .map(|(id, e)| ((*id).to_owned(), e.clone()))
        .collect()
}

#[test]
fn resolves_a_bare_or_bracketed_handle() {
    let by_id = handles(&[("evt_3", with("t", "x", "2026-06-17"))]);
    assert_eq!(
        resolve_explicit_event_reference(Some("evt_3"), &by_id).map(|e| e.event_hash.as_str()),
        Some("t")
    );
    assert_eq!(
        resolve_explicit_event_reference(Some("[evt_3]"), &by_id).map(|e| e.event_hash.as_str()),
        Some("t")
    );
}

#[test]
fn returns_undefined_without_an_eventid_no_title_fallback_for_cancels() {
    let by_id = handles(&[("evt_3", with("t", "x", "2026-06-17"))]);
    assert_eq!(resolve_explicit_event_reference(None, &by_id), None);
    assert_eq!(resolve_explicit_event_reference(Some(""), &by_id), None);
}

#[test]
fn returns_undefined_for_an_unknown_handle() {
    let by_id = handles(&[("evt_3", with("t", "x", "2026-06-17"))]);
    assert_eq!(
        resolve_explicit_event_reference(Some("evt_99"), &by_id),
        None
    );
}

// describe("pickByStartDate")

#[test]
fn returns_the_exact_startdate_match_when_present() {
    let a = with("a", "x", "2026-06-17");
    let b = with("b", "x", "2026-09-01");
    assert_eq!(pick_by_start_date(&[&a, &b], "2026-09-01"), Some(&b));
}

#[test]
fn returns_a_lone_candidate_when_no_date_matches() {
    let a = with("a", "x", "2026-06-17");
    assert_eq!(pick_by_start_date(&[&a], "2099-01-01"), Some(&a));
}

#[test]
fn fails_closed_undefined_when_several_share_the_title_and_none_match_the_date() {
    let a = with("a", "x", "2026-06-17");
    let b = with("b", "x", "2026-09-01");
    assert_eq!(pick_by_start_date(&[&a, &b], "2099-01-01"), None);
}

#[test]
fn returns_undefined_for_no_candidates() {
    assert_eq!(pick_by_start_date(&[], "2026-06-17"), None);
}

// describe("resolveEventReference")

#[test]
fn prefers_the_eventid_handle_over_a_fallback_that_would_also_match() {
    let target = CreatedCalendarEvent {
        calendar_event_id: "cal-1".to_owned(),
        ..with("by-id", "x", "2026-06-17")
    };
    let other = CreatedCalendarEvent {
        calendar_event_id: "cal-2".to_owned(),
        ..with("by-fallback", "x", "2026-06-17")
    };
    let by_id = handles(&[("evt_2", target)]);
    let called = Cell::new(false);
    let fallback = |_: &str, _: &str| {
        called.set(true);
        Some(&other)
    };
    let result = resolve_event_reference(
        Some("evt_2"),
        "completely different title",
        "2099-01-01",
        &by_id,
        Some(&fallback),
    );
    assert_eq!(result.map(|e| e.event_hash.as_str()), Some("by-id"));
    assert!(!called.get());
}

#[test]
fn tolerates_a_handle_echoed_back_with_surrounding_brackets() {
    let by_id = handles(&[("evt_2", with("by-id", "x", "2026-06-17"))]);
    let none = |_: &str, _: &str| None;
    let result = resolve_event_reference(Some("[evt_2]"), "x", "2026-06-17", &by_id, Some(&none));
    assert_eq!(result.map(|e| e.event_hash.as_str()), Some("by-id"));
}

#[test]
fn falls_back_to_title_startdate_when_eventid_is_missing() {
    let found = with("by-title", "x", "2026-06-17");
    let seen = std::cell::RefCell::new(Vec::new());
    let fallback = |title: &str, date: &str| {
        seen.borrow_mut().push((title.to_owned(), date.to_owned()));
        Some(&found)
    };
    let by_id = EventHandles::new();
    let result = resolve_event_reference(
        None,
        "🦷 Dentist Appointment",
        "2026-06-17",
        &by_id,
        Some(&fallback),
    );
    assert_eq!(result.map(|e| e.event_hash.as_str()), Some("by-title"));
    assert_eq!(
        seen.into_inner(),
        [("🦷 Dentist Appointment".to_owned(), "2026-06-17".to_owned())]
    );
}

#[test]
fn falls_back_when_eventid_is_hallucinated_not_in_the_map() {
    let found = with("by-title", "x", "2026-06-17");
    let calls = Cell::new(0);
    let fallback = |_: &str, _: &str| {
        calls.set(calls.get() + 1);
        Some(&found)
    };
    let by_id = handles(&[("evt_1", rec())]);
    let result = resolve_event_reference(
        Some("evt_99"),
        "🦷 Dentist Appointment",
        "2026-06-17",
        &by_id,
        Some(&fallback),
    );
    assert_eq!(result.map(|e| e.event_hash.as_str()), Some("by-title"));
    assert_eq!(calls.get(), 1);
}

#[test]
fn defaults_the_fallback_to_title_date_search_over_the_windowed_byid_values() {
    let by_id = handles(&[("evt_1", with("windowed", "💇 Haircut", "2026-07-20"))]);
    assert_eq!(
        resolve_event_reference(None, "Haircut", "2026-07-20", &by_id, None)
            .map(|e| e.event_hash.as_str()),
        Some("windowed")
    );
    assert_eq!(
        resolve_event_reference(None, "Unrelated", "2026-07-20", &by_id, None),
        None
    );
}

#[test]
fn returns_undefined_when_neither_the_handle_nor_the_fallback_matches() {
    let none = |_: &str, _: &str| None;
    let by_id = EventHandles::new();
    assert_eq!(
        resolve_event_reference(Some("evt_99"), "Unknown", "2026-06-17", &by_id, Some(&none)),
        None
    );
}

// Store round trips.

async fn store() -> TestStore {
    TestStore::new(test_clock(TEST_EPOCH_MS)).await
}

#[tokio::test]
async fn cancelled_records_are_not_active_duplicates_but_stay_tracked() {
    let t = store().await;
    let event = with("dentist|2026-09-10|allday", "Dentist", "2026-09-10");
    persistence::record_created_event(&t.store, event.clone())
        .await
        .unwrap();
    assert!(
        persistence::has_created_event(&t.store, &event.event_hash)
            .await
            .unwrap()
    );
    persistence::mark_event_cancelled(&t.store, &event.event_hash)
        .await
        .unwrap();
    assert!(
        !persistence::has_created_event(&t.store, &event.event_hash)
            .await
            .unwrap()
    );
    let stored = persistence::get_tracked_event(&t.store, &event.event_hash)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.status, Some(EventStatus::Cancelled));
}

#[tokio::test]
async fn cancelling_keeps_unknown_fields_and_field_order() {
    let t = store().await;
    let pk =
        omni_store::entity::pk::<CreatedCalendarEvent>(&"x|2026-01-02|allday".to_owned()).unwrap();
    let value = cbor::decode(&cbor::encode(&JsValue::Object(
        [
            (
                "eventHash",
                JsValue::String("x|2026-01-02|allday".to_owned()),
            ),
            ("emailId", JsValue::String("e".to_owned())),
            ("calendarEventId", JsValue::String("c".to_owned())),
            ("title", JsValue::String("X".to_owned())),
            ("startDate", JsValue::String("2026-01-02".to_owned())),
            ("startTime", JsValue::Undefined),
            ("allDay", JsValue::Bool(true)),
            ("futureField", JsValue::Int(7)),
            ("createdAt", JsValue::Int(1)),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v))
        .collect(),
    )))
    .unwrap();
    let pk2 = pk.clone();
    t.store
        .write(move |tx| {
            omni_store::DocWrite::upsert_doc(
                tx,
                &pk2,
                &value,
                omni_store::DocMeta {
                    entity: Some(CreatedCalendarEvent::NAME.to_owned()),
                    version: 0,
                    expires_at: None,
                    updated_at: Some(1),
                },
            )
        })
        .await
        .unwrap();
    persistence::mark_event_cancelled(&t.store, "x|2026-01-02|allday")
        .await
        .unwrap();
    let stored = t
        .store
        .read(move |docs| docs.get_doc(&pk))
        .await
        .unwrap()
        .unwrap();
    let keys: Vec<&str> = stored
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        [
            "eventHash",
            "emailId",
            "calendarEventId",
            "title",
            "startDate",
            "allDay",
            "futureField",
            "createdAt",
            "status"
        ]
    );
    // The `undefined` startTime is not written.
    assert_eq!(stored.get("startTime"), None);
    assert_eq!(stored.get("futureField"), Some(&JsValue::Int(7)));
}

#[tokio::test]
async fn reconcile_rekeys_legacy_hashes_once() {
    let t = store().await;
    let legacy = with(
        "✈️ flight yyz → yvr|2026-07-02|08:00",
        "✈️ Flight YYZ → YVR",
        "2026-07-02",
    );
    let legacy = CreatedCalendarEvent {
        start_time: Some("08:00".to_owned()),
        ..legacy
    };
    persistence::record_created_event(&t.store, legacy)
        .await
        .unwrap();
    assert_eq!(
        persistence::reconcile_event_hashes(&t.store).await.unwrap(),
        1
    );
    assert_eq!(
        persistence::reconcile_event_hashes(&t.store).await.unwrap(),
        0
    );
    let all = persistence::get_tracked_events(&t.store).await.unwrap();
    assert_eq!(hashes(&all), ["flight yyz yvr|2026-07-02|08:00"]);
}

#[tokio::test]
async fn replace_tombstones_the_previous_identity() {
    let t = store().await;
    let old = with("old", "Dentist", "2026-09-10");
    persistence::record_created_event(&t.store, old)
        .await
        .unwrap();
    let new = with("new", "Dentist moved", "2026-09-11");
    persistence::replace_created_event(&t.store, new, "old")
        .await
        .unwrap();
    let old = persistence::get_tracked_event(&t.store, "old")
        .await
        .unwrap()
        .unwrap();
    assert!(old.is_cancelled());
    assert!(
        persistence::has_created_event(&t.store, "new")
            .await
            .unwrap()
    );
}

#[test]
fn a_create_is_blocked_only_for_the_exact_event_and_the_right_reason() {
    assert_eq!(create_block(None, "e"), None);
    let active = rec();
    assert_eq!(
        create_block(Some(&active), "other"),
        Some(CreateBlock::Duplicate)
    );

    // Cancelled or moved by an email: only an email that wrote the row is
    // stale; another email may book the slot again.
    let mut cancelled = rec();
    cancelled.status = Some(EventStatus::Cancelled);
    cancelled.earlier_email_ids = vec!["first".to_owned()];
    for writer in ["e", "first"] {
        assert_eq!(
            create_block(Some(&cancelled), writer),
            Some(CreateBlock::Superseded)
        );
    }
    assert_eq!(create_block(Some(&cancelled), "other"), None);
    assert_eq!(cancelled.writers(), ["first", "e"]);

    // Deleted from the calendar: no email recreates it.
    let mut deleted = cancelled.clone();
    deleted.deleted_at = Some(1);
    for email in ["e", "other"] {
        assert_eq!(
            create_block(Some(&deleted), email),
            Some(CreateBlock::Deleted)
        );
    }
    // `deletedAt` without the cancelled status is not a tombstone.
    let mut odd = rec();
    odd.deleted_at = Some(1);
    assert!(!odd.is_deleted());
}

#[tokio::test]
async fn a_calendar_deletion_tombstones_the_active_row_of_that_event_only() {
    let t = store().await;
    // One calendar event that was rescheduled once, and an unrelated one.
    persistence::record_created_event(&t.store, with("before", "Dentist", "2026-09-10"))
        .await
        .unwrap();
    let mut after = with("after", "Dentist", "2026-09-11");
    after
        .extra
        .insert("futureField".to_owned(), JsValue::Bool(true));
    persistence::replace_created_event(&t.store, after, "before")
        .await
        .unwrap();
    let mut other = with("other", "Vet", "2026-09-12");
    other.calendar_event_id = "another".to_owned();
    persistence::record_created_event(&t.store, other)
        .await
        .unwrap();

    assert_eq!(
        persistence::mark_calendar_event_deleted(&t.store, "cal")
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        persistence::mark_calendar_event_deleted(&t.store, "cal")
            .await
            .unwrap(),
        0,
        "idempotent"
    );
    let get = |hash: &'static str| {
        let store = t.store.clone();
        async move {
            persistence::get_tracked_event(&store, hash)
                .await
                .unwrap()
                .unwrap()
        }
    };
    // The time it was moved away from is a different event: still only
    // cancelled, so another email may book it.
    let before = get("before").await;
    assert!(before.is_cancelled() && !before.is_deleted());
    let after = get("after").await;
    assert!(after.is_deleted());
    assert!(after.deleted_at.is_some_and(|at| at >= TEST_EPOCH_MS));
    assert_eq!(after.title, "Dentist", "other fields are kept");
    assert_eq!(after.extra.get("futureField"), Some(&JsValue::Bool(true)));
    assert!(!get("other").await.is_cancelled());
    assert!(
        !persistence::has_created_event(&t.store, "after")
            .await
            .unwrap()
    );

    // A later booking of the same slot by the pipeline is an active row again.
    persistence::record_created_event(&t.store, with("after", "Dentist", "2026-09-11"))
        .await
        .unwrap();
    assert!(!get("after").await.is_deleted());
}
