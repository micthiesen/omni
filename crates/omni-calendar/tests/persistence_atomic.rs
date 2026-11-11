//! Calendar event transactions: forced SQLite failures roll back the whole
//! operation.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use omni_calendar::persistence::{self, CreatedCalendarEvent, compute_event_hash};
use omni_store::cbor::Extra;
use omni_store::entity::pk;
use omni_store::{Store, StoreError};
use omni_testkit::{TEST_EPOCH_MS, TestStore, test_clock};

fn event(event_hash: &str, title: &str) -> CreatedCalendarEvent {
    CreatedCalendarEvent {
        event_hash: event_hash.to_owned(),
        email_id: "email-1".to_owned(),
        calendar_event_id: "caldav-1".to_owned(),
        title: title.to_owned(),
        start_date: "2026-09-10".to_owned(),
        start_time: None,
        end_date: None,
        end_time: None,
        all_day: Some(true),
        location: None,
        time_zone: None,
        description: None,
        duration: None,
        reminder_minutes: None,
        recurrence: None,
        created_at: 1_800_000_000_000,
        status: None,
        deleted_at: None,
        earlier_email_ids: Vec::new(),
        extra: Extra::default(),
    }
}

fn sql_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

async fn exec(store: &Store, sql: String) {
    store
        .write(move |tx| {
            tx.connection()
                .execute_batch(&sql)
                .map_err(|e| StoreError::Sqlite(e.to_string()))
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn rolls_back_the_replacement_when_cancelling_the_prior_row_fails() {
    let t = TestStore::new(test_clock(TEST_EPOCH_MS)).await;
    let prior = event("old-event", "Dentist");
    let replacement = event("new-event", "Dentist moved");
    persistence::record_created_event(&t.store, prior.clone())
        .await
        .unwrap();
    let prior_pk = pk::<CreatedCalendarEvent>(&prior.event_hash).unwrap();
    exec(
        &t.store,
        format!(
            "CREATE TRIGGER fail_calendar_cancel BEFORE UPDATE ON blobs WHEN NEW.pk = {} BEGIN SELECT RAISE(ABORT, 'forced'); END",
            sql_literal(&prior_pk)
        ),
    )
    .await;

    let result =
        persistence::replace_created_event(&t.store, replacement.clone(), &prior.event_hash).await;
    exec(&t.store, "DROP TRIGGER fail_calendar_cancel".to_owned()).await;

    assert!(result.is_err());
    assert!(
        persistence::get_tracked_event(&t.store, &replacement.event_hash)
            .await
            .unwrap()
            .is_none()
    );
    let stored = persistence::get_tracked_event(&t.store, &prior.event_hash)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.status, None);
}

#[tokio::test]
async fn rolls_back_a_rekey_when_deleting_the_legacy_row_fails() {
    let t = TestStore::new(test_clock(TEST_EPOCH_MS)).await;
    let prior = event("legacy-hash", "Dentist");
    let expected = compute_event_hash(&prior.title, &prior.start_date, prior.start_time.as_deref());
    persistence::record_created_event(&t.store, prior.clone())
        .await
        .unwrap();
    let prior_pk = pk::<CreatedCalendarEvent>(&prior.event_hash).unwrap();
    exec(
        &t.store,
        format!(
            "CREATE TRIGGER fail_calendar_rekey BEFORE DELETE ON blobs WHEN OLD.pk = {} BEGIN SELECT RAISE(ABORT, 'forced'); END",
            sql_literal(&prior_pk)
        ),
    )
    .await;

    let result = persistence::reconcile_event_hashes(&t.store).await;
    exec(&t.store, "DROP TRIGGER fail_calendar_rekey".to_owned()).await;

    assert!(result.is_err());
    assert!(
        persistence::get_tracked_event(&t.store, &expected)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        persistence::get_tracked_event(&t.store, &prior.event_hash)
            .await
            .unwrap()
            .is_some()
    );
}
