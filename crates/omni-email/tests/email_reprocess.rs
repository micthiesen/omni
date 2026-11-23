//! The `email_reprocess` tool and `reprocess_activity`.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use omni_core::email::{EmailHandler, HandlerError};
use omni_email::activity::{self, EmailActivityOutcome, EmailPipelineName, NewActivity};
use omni_email::reprocess::{ReprocessFailure, handle_then_clear_retry, reprocess_activity};
use omni_email::retry;
use omni_runtime::Ports;

use common::{FakeReader, NOW, email, fn_handler, handlers, store_at};

#[tokio::test]
async fn clears_the_scheduled_retry_only_after_successful_processing() {
    let order = Arc::new(AtomicUsize::new(0));
    let handled_at = Arc::new(AtomicUsize::new(0));
    let (counter, handled) = (order.clone(), handled_at.clone());
    let handler = fn_handler("ParcelTracker", move |emails| {
        assert_eq!(emails.len(), 1);
        handled.store(counter.fetch_add(1, Ordering::SeqCst) + 1, Ordering::SeqCst);
        Box::pin(async { Ok::<(), HandlerError>(()) })
    });
    let cleared_at = Arc::new(AtomicUsize::new(0));
    let (counter, cleared) = (order.clone(), cleared_at.clone());
    handle_then_clear_retry(handler.as_ref(), &email("message-1"), || async move {
        cleared.store(counter.fetch_add(1, Ordering::SeqCst) + 1, Ordering::SeqCst);
        Ok(())
    })
    .await
    .unwrap();
    assert_eq!(handled_at.load(Ordering::SeqCst), 1);
    assert_eq!(cleared_at.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn retains_the_scheduled_retry_when_processing_fails() {
    let handler = fn_handler("ParcelTracker", |_| {
        Box::pin(async { Err::<(), _>(HandlerError::permanent("pipeline failed", None)) })
    });
    let cleared = Arc::new(AtomicUsize::new(0));
    let counter = cleared.clone();
    let result = handle_then_clear_retry(handler.as_ref(), &email("message-1"), || async move {
        counter.fetch_add(1, Ordering::SeqCst);
        Ok(())
    })
    .await;
    assert!(result.is_err());
    assert_eq!(cleared.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn reprocess_reruns_the_pipeline_and_clears_the_retry() {
    let (store, _clock) = store_at(NOW).await;
    let mut fetched = email("m1");
    fetched.subject = "Shipment".to_owned();
    activity::record(
        &store.store,
        NewActivity::new(
            EmailPipelineName::ParcelTracker,
            &fetched,
            EmailActivityOutcome::Error,
        ),
    )
    .await
    .unwrap();
    retry::enqueue(&store.store, "ParcelTracker", "m1", "503")
        .await
        .unwrap();

    let ports = Ports::default();
    assert!(matches!(
        reprocess_activity(&store.store, &ports, "ParcelTracker#m1").await,
        Err(ReprocessFailure::PipelinesInactive)
    ));
    ports
        .set_email_reader(FakeReader::new(|id| Ok(Some(email(id)))))
        .ok()
        .unwrap();
    let inner = store.store.clone();
    let handler: Arc<dyn EmailHandler> = fn_handler("ParcelTracker", move |emails| {
        let store = inner.clone();
        Box::pin(async move {
            activity::record(
                &store,
                NewActivity::new(
                    EmailPipelineName::ParcelTracker,
                    &emails[0],
                    EmailActivityOutcome::Processed,
                ),
            )
            .await
            .map(|_| ())
            .map_err(|e| HandlerError::transient(e.to_string(), None))
        })
    });
    ports
        .set_email_retry_handlers(handlers(vec![("ParcelTracker", handler)]))
        .ok()
        .unwrap();

    assert!(matches!(
        reprocess_activity(&store.store, &ports, "ParcelTracker#nope").await,
        Err(ReprocessFailure::UnknownActivity(_))
    ));
    let updated = reprocess_activity(&store.store, &ports, "ParcelTracker#m1")
        .await
        .unwrap();
    assert_eq!(updated.outcome, EmailActivityOutcome::Processed);
    assert!(
        retry::get(&store.store, "ParcelTracker#m1")
            .await
            .unwrap()
            .is_none()
    );
}
