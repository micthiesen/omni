//! The Reminders service.
//!
//! Background registration is synchronous (the job is handed to the application's
//! task tracker before any await), so the interruption case checks that aborting
//! the request after registration still completes the initial snapshot.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::type_complexity)]

use crate::common;

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::BoxFuture;
use omni_api::reminders::{Diagnostic, DiagnosticCategory, DiagnosticStage, Phase, Reason};
use omni_reminders::apple::{
    AppleApi, AppleBeginResult, AppleErrorKind, AppleRemindersError, CkPath,
};
use omni_reminders::cloudkit::ReminderCreateFields;
use omni_reminders::codec::encode_crdt_document;
use omni_reminders::config::RemindersConfiguration;
use omni_reminders::protected_access::ProtectedAccess;
use omni_reminders::recurring_completion::RecurringCompletionTarget;
use omni_reminders::service::{
    Background, RemindersService, ServiceDeps, ServiceError, ServiceErrorCode,
};
use omni_reminders::store::{
    OperationState, RemindersStore, RemindersStoreError, StoredState, empty_reminders_state,
};
use serde_json::{Value, json};
use tokio_util::task::TaskTracker;

fn config() -> RemindersConfiguration {
    RemindersConfiguration {
        enabled: Some("true".into()),
        account: Some("test@example.com".into()),
        password: Some("never-sent".into()),
        storage_key: Some("a".repeat(64)),
        public_origin: Some("https://omni.example.test".into()),
        directory: "/tmp/omni-reminders-service-test-unused".into(),
    }
}

#[derive(Default)]
struct MemoryStore {
    state: Mutex<StoredState>,
    reads: AtomicUsize,
    writes: AtomicUsize,
    fail_writes: AtomicBool,
}

impl RemindersStore for MemoryStore {
    fn read(&self) -> BoxFuture<'_, Result<StoredState, RemindersStoreError>> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        let state = self.state.lock().unwrap().clone();
        Box::pin(async move { Ok(state) })
    }

    fn write(&self, state: StoredState) -> BoxFuture<'_, Result<(), RemindersStoreError>> {
        self.writes.fetch_add(1, Ordering::SeqCst);
        if self.fail_writes.load(Ordering::SeqCst) {
            return Box::pin(async {
                Err(RemindersStoreError {
                    operation: omni_reminders::store::StoreOperation::Write,
                })
            });
        }
        *self.state.lock().unwrap() = state;
        Box::pin(async { Ok(()) })
    }
}

type AppleFn<T> = Box<dyn Fn() -> BoxFuture<'static, Result<T, AppleRemindersError>> + Send + Sync>;
type CkFn = Box<
    dyn Fn(&str, Value) -> BoxFuture<'static, Result<Value, AppleRemindersError>> + Send + Sync,
>;

struct FakeApple {
    begin: AppleFn<AppleBeginResult>,
    verify: AppleFn<bool>,
    submit: AppleFn<()>,
    ck: CkFn,
    begins: AtomicUsize,
    verifies: AtomicUsize,
    submits: AtomicUsize,
    pcs: AtomicUsize,
    ck_calls: Mutex<Vec<(String, Value)>>,
}

impl FakeApple {
    fn ck_paths(&self) -> Vec<String> {
        self.ck_calls
            .lock()
            .unwrap()
            .iter()
            .map(|(p, _)| p.clone())
            .collect()
    }
}

impl AppleApi for FakeApple {
    fn begin(&self) -> BoxFuture<'_, Result<AppleBeginResult, AppleRemindersError>> {
        self.begins.fetch_add(1, Ordering::SeqCst);
        (self.begin)()
    }

    fn verify(&self) -> BoxFuture<'_, Result<bool, AppleRemindersError>> {
        self.verifies.fetch_add(1, Ordering::SeqCst);
        (self.verify)()
    }

    fn submit_2fa<'a>(&'a self, _code: &'a str) -> BoxFuture<'a, Result<(), AppleRemindersError>> {
        self.submits.fetch_add(1, Ordering::SeqCst);
        (self.submit)()
    }

    fn request_pcs_access(&self) -> BoxFuture<'_, Result<ProtectedAccess, AppleRemindersError>> {
        self.pcs.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(ProtectedAccess::NotRequired) })
    }

    fn ck_post(
        &self,
        path: CkPath,
        body: Value,
    ) -> BoxFuture<'_, Result<Value, AppleRemindersError>> {
        self.ck_calls
            .lock()
            .unwrap()
            .push((path.as_str().to_owned(), body.clone()));
        (self.ck)(path.as_str(), body)
    }
}

fn ready<T: Send + 'static>(value: T) -> BoxFuture<'static, Result<T, AppleRemindersError>> {
    Box::pin(async move { Ok(value) })
}

fn auth_error(kind: AppleErrorKind) -> AppleRemindersError {
    AppleRemindersError::new("fixture", "opaque", None, kind)
}

struct Options {
    begin: AppleFn<AppleBeginResult>,
    verify: AppleFn<bool>,
    submit: AppleFn<()>,
    ck: CkFn,
    background: Option<Background>,
    tracker: Option<TaskTracker>,
    diagnostics: Option<Arc<Mutex<Vec<Diagnostic>>>>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            begin: Box::new(|| ready(AppleBeginResult::MfaRequired)),
            verify: Box::new(|| ready(false)),
            submit: Box::new(|| ready(())),
            ck: Box::new(|_, _| ready(json!({"zones": [{"records": []}]}))),
            background: None,
            tracker: None,
            diagnostics: None,
        }
    }
}

struct Fixture {
    service: RemindersService,
    store: Arc<MemoryStore>,
    apple: Arc<FakeApple>,
    notifications: Arc<AtomicUsize>,
}

impl Fixture {
    fn restart(&self) -> RemindersService {
        self.restart_with(None)
    }

    fn restart_with(&self, notifications: Option<Arc<AtomicUsize>>) -> RemindersService {
        build(
            &config(),
            self.store.clone(),
            self.apple.clone(),
            notifications.unwrap_or_default(),
            None,
            None,
            None,
        )
    }

    fn state(&self) -> StoredState {
        self.store.state.lock().unwrap().clone()
    }
}

fn build(
    config: &RemindersConfiguration,
    store: Arc<MemoryStore>,
    apple: Arc<FakeApple>,
    notifications: Arc<AtomicUsize>,
    background: Option<Background>,
    tracker: Option<TaskTracker>,
    diagnostics: Option<Arc<Mutex<Vec<Diagnostic>>>>,
) -> RemindersService {
    RemindersService::new(
        config,
        ServiceDeps {
            store,
            apple: Box::new(move |_| apple as Arc<dyn AppleApi>),
            notify: Arc::new(move || {
                notifications.fetch_add(1, Ordering::SeqCst);
                Box::pin(async { Ok(()) })
            }),
            log_failure: diagnostics.map(|d| {
                Arc::new(move |diagnostic: &Diagnostic| d.lock().unwrap().push(*diagnostic))
                    as omni_reminders::service::FailureLogger
            }),
            background,
            tracker,
            clock: omni_testkit::test_clock(common::NOW),
        },
    )
}

fn fixture(options: Options) -> Fixture {
    let store = Arc::new(MemoryStore {
        state: Mutex::new(empty_reminders_state()),
        ..MemoryStore::default()
    });
    let apple = Arc::new(FakeApple {
        begin: options.begin,
        verify: options.verify,
        submit: options.submit,
        ck: options.ck,
        begins: AtomicUsize::new(0),
        verifies: AtomicUsize::new(0),
        submits: AtomicUsize::new(0),
        pcs: AtomicUsize::new(0),
        ck_calls: Mutex::new(Vec::new()),
    });
    let notifications = Arc::new(AtomicUsize::new(0));
    let service = build(
        &config(),
        store.clone(),
        apple.clone(),
        notifications.clone(),
        options.background,
        options.tracker,
        options.diagnostics,
    );
    Fixture {
        service,
        store,
        apple,
        notifications,
    }
}

/// A background seam that queues jobs for the test to run.
fn queued() -> (Background, Arc<Mutex<Vec<BoxFuture<'static, ()>>>>) {
    let jobs: Arc<Mutex<Vec<BoxFuture<'static, ()>>>> = Arc::default();
    let sink = jobs.clone();
    (Arc::new(move |job| sink.lock().unwrap().push(job)), jobs)
}

fn take(jobs: &Arc<Mutex<Vec<BoxFuture<'static, ()>>>>) -> BoxFuture<'static, ()> {
    jobs.lock().unwrap().remove(0)
}

fn service_code(error: &ServiceError) -> Option<ServiceErrorCode> {
    match error {
        ServiceError::Service(e) => Some(e.code),
        _ => None,
    }
}

fn encrypted_snapshot(encrypted: Arc<AtomicBool>) -> CkFn {
    Box::new(move |path, body| {
        let enc = encrypted.load(Ordering::SeqCst);
        let record = json!({
            "recordName": "fixture",
            "recordType": "Reminder",
            "recordChangeTag": "tag",
            "fields": {"TitleDocument": {"type": "ENCRYPTED_BYTES", "value": "opaque"}},
        });
        let records = if enc { json!([record]) } else { json!([]) };
        ready(
            if path == "/changes/zone" && body["zones"][0]["reverse"] == json!(true) {
                json!({"zones": [{"records": records}]})
            } else if path == "/changes/zone" {
                json!({"zones": [{"records": [{"recordName": "List/test", "recordType": "List", "fields": {"Name": {"type": "STRING", "value": "Test"}}}]}]})
            } else {
                json!({"records": records})
            },
        )
    })
}

#[tokio::test]
async fn reports_a_missing_completed_cursor_instead_of_synchronizing_forever() {
    let (background, jobs) = queued();
    let x = fixture(Options {
        verify: Box::new(|| ready(true)),
        background: Some(background),
        ..Options::default()
    });
    x.service.verify_access().await;
    take(&jobs).await;
    assert_eq!(x.service.status().phase, Phase::UnsupportedProtocol);
    assert!(x.service.snapshot().await.is_err());
    assert!(jobs.lock().unwrap().is_empty());
}

#[tokio::test]
async fn rebuilds_an_unusable_cursor_on_the_next_access_check_without_signing_in() {
    let (background, jobs) = queued();
    let fail_refresh = Arc::new(AtomicBool::new(false));
    let full_scans = Arc::new(AtomicUsize::new(0));
    let (failing, scans) = (fail_refresh.clone(), full_scans.clone());
    let x = fixture(Options {
        verify: Box::new(|| ready(true)),
        background: Some(background),
        ck: Box::new(move |_, body| {
            let zone = &body["zones"][0];
            if zone["reverse"] == json!(true) {
                return ready(json!({"zones": [{"records": []}]}));
            }
            let has_token = zone.get("syncToken").is_some();
            if !has_token {
                scans.fetch_add(1, Ordering::SeqCst);
            }
            ready(if failing.load(Ordering::SeqCst) && has_token {
                json!({"zones": [{"error": {"serverErrorCode": "UNKNOWN_CURSOR_FAILURE"}}]})
            } else {
                json!({"zones": [{"records": [], "syncToken": "complete"}]})
            })
        }),
        ..Options::default()
    });
    x.service.verify_access().await;
    take(&jobs).await;
    fail_refresh.store(true, Ordering::SeqCst);
    assert!(x.service.snapshot().await.is_err());
    assert_eq!(x.service.status().phase, Phase::UnsupportedProtocol);
    fail_refresh.store(false, Ordering::SeqCst);
    x.service.verify_access().await;
    assert_eq!(jobs.lock().unwrap().len(), 1);
    take(&jobs).await;
    let snapshot = x.service.snapshot().await.unwrap();
    assert!(snapshot.lists.is_empty() && snapshot.reminders.is_empty());
    assert_eq!(full_scans.load(Ordering::SeqCst), 2);
    assert_eq!(x.apple.begins.load(Ordering::SeqCst), 0);
}

#[tokio::test(start_paused = true)]
async fn rechecks_initialization_for_reads_queued_before_authentication_completes() {
    let (background, jobs) = queued();
    let x = fixture(Options {
        verify: Box::new(|| {
            Box::pin(async {
                tokio::time::sleep(Duration::from_secs(1)).await;
                Ok(true)
            })
        }),
        background: Some(background),
        ..Options::default()
    });
    let auth_service = x.service.clone();
    let auth = tokio::spawn(async move { auth_service.verify_access().await });
    tokio::time::sleep(Duration::from_millis(500)).await;
    let read_service = x.service.clone();
    let read = tokio::spawn(async move { read_service.snapshot().await });
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(auth.await.unwrap().phase, Phase::Authenticated);
    let error = read.await.unwrap().unwrap_err();
    assert_eq!(service_code(&error), Some(ServiceErrorCode::Synchronizing));
    assert_eq!(jobs.lock().unwrap().len(), 1);
    assert_eq!(x.apple.ck_calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn registers_the_background_task_even_if_the_initiating_request_is_interrupted() {
    let tracker = TaskTracker::new();
    let registered = Arc::new(tokio::sync::Notify::new());
    let (spawn_on, signal) = (tracker.clone(), registered.clone());
    let background: Background = Arc::new(move |job| {
        spawn_on.spawn(job);
        signal.notify_one();
    });
    let x = fixture(Options {
        verify: Box::new(|| ready(true)),
        background: Some(background),
        ck: Box::new(|_, _| ready(json!({"zones": [{"records": [], "syncToken": "complete"}]}))),
        ..Options::default()
    });
    let service = x.service.clone();
    let request = tokio::spawn(async move { service.verify_access().await });
    registered.notified().await;
    request.abort();
    tracker.close();
    tracker.wait().await;
    let snapshot = x.service.snapshot().await.unwrap();
    assert!(snapshot.lists.is_empty() && snapshot.reminders.is_empty());
}

#[tokio::test(start_paused = true)]
async fn keeps_initial_synchronization_in_the_application_scope_and_fails_fast_while_loading() {
    let tracker = TaskTracker::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let (spawn_on, count) = (tracker.clone(), calls.clone());
    let background: Background = Arc::new(move |job| {
        count.fetch_add(1, Ordering::SeqCst);
        spawn_on.spawn(job);
    });
    let x = fixture(Options {
        verify: Box::new(|| ready(true)),
        background: Some(background),
        ck: Box::new(|_, body| {
            let zone = body["zones"][0].clone();
            Box::pin(async move {
                if zone["reverse"] == json!(true) {
                    return Ok(json!({"zones": [{"records": []}]}));
                }
                if zone.get("syncToken").is_none() {
                    tokio::time::sleep(Duration::from_secs(120)).await;
                }
                Ok(json!({"zones": [{"records": [], "syncToken": "complete"}]}))
            })
        }),
        ..Options::default()
    });
    assert_eq!(x.service.verify_access().await.phase, Phase::Authenticated);
    let loading = x.service.snapshot().await.unwrap_err();
    assert_eq!(
        service_code(&loading),
        Some(ServiceErrorCode::Synchronizing)
    );
    let lookup = x.service.get("Reminder/test").await.unwrap_err();
    assert_eq!(service_code(&lookup), Some(ServiceErrorCode::Synchronizing));
    assert_eq!(x.service.verify_access().await.phase, Phase::Authenticated);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    tokio::time::sleep(Duration::from_secs(121)).await;
    let snapshot = x.service.snapshot().await.unwrap();
    assert!(snapshot.lists.is_empty() && snapshot.reminders.is_empty());
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn reports_accepted_code_separately_from_protected_data_access() {
    let x = fixture(Options {
        verify: Box::new(|| ready(true)),
        ck: encrypted_snapshot(Arc::new(AtomicBool::new(true))),
        ..Options::default()
    });
    let challenge = x.service.start_authentication().await.unwrap();
    let status = x
        .service
        .submit_code(challenge.challenge_id.as_deref().unwrap(), "123456")
        .await
        .unwrap();
    assert_eq!(
        (status.phase, status.reason),
        (Phase::AwaitingDeviceApproval, Some(Reason::Pcs))
    );
    assert!(status.challenge_id.is_none());
    assert_eq!(x.apple.submits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn keeps_encrypted_data_awaiting_approval_without_repeating_sign_in_or_consent() {
    let encrypted = Arc::new(AtomicBool::new(true));
    let x = fixture(Options {
        verify: Box::new(|| ready(true)),
        ck: encrypted_snapshot(encrypted.clone()),
        ..Options::default()
    });
    x.service.health_check().await;
    let status = x.service.status();
    assert_eq!(
        (status.phase, status.reason),
        (Phase::AwaitingDeviceApproval, Some(Reason::Pcs))
    );
    x.service.health_check().await;
    assert_eq!(x.apple.ck_calls.lock().unwrap().len(), 1);
    assert_eq!(x.apple.pcs.load(Ordering::SeqCst), 0);
    assert_eq!(x.apple.begins.load(Ordering::SeqCst), 0);
    let blocked = x.service.snapshot().await.unwrap_err();
    assert!(
        blocked.to_string().contains("awaiting-device-approval"),
        "{blocked}"
    );
    encrypted.store(false, Ordering::SeqCst);
    assert_eq!(x.service.verify_access().await.phase, Phase::Authenticated);
    assert_eq!(x.apple.pcs.load(Ordering::SeqCst), 1);
    assert_eq!(x.apple.ck_calls.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn distinguishes_mfa_options_push_and_verify_failures() {
    for (operation, stage) in [
        ("MFA options", DiagnosticStage::SecondFactorOptions),
        ("MFA push", DiagnosticStage::DeviceNotification),
        ("MFA verify", DiagnosticStage::CodeVerification),
    ] {
        let x = fixture(Options {
            begin: Box::new(move || {
                Box::pin(async move {
                    Err(AppleRemindersError::new(
                        operation,
                        "private response",
                        Some(405),
                        AppleErrorKind::UnsupportedProtocol,
                    ))
                })
            }),
            ..Options::default()
        });
        assert!(x.service.start_authentication().await.is_err());
        assert_eq!(
            x.service.status().diagnostic,
            Some(Diagnostic {
                stage,
                category: DiagnosticCategory::AppleResponse,
                http_status: Some(405)
            })
        );
    }
}

#[tokio::test]
async fn reports_and_logs_only_bounded_sign_in_diagnostics() {
    let diagnostics = Arc::new(Mutex::new(Vec::new()));
    let x = fixture(Options {
        begin: Box::new(|| {
            Box::pin(async {
                Err(AppleRemindersError::new(
                    "SRP init",
                    "secret upstream cookies",
                    Some(503),
                    AppleErrorKind::TransientOutage,
                ))
            })
        }),
        diagnostics: Some(diagnostics.clone()),
        ..Options::default()
    });
    assert!(x.service.start_authentication().await.is_err());
    let expected = Diagnostic {
        stage: DiagnosticStage::SignInInit,
        category: DiagnosticCategory::AppleResponse,
        http_status: Some(503),
    };
    assert_eq!(x.service.status().diagnostic, Some(expected));
    assert_eq!(*diagnostics.lock().unwrap(), vec![expected]);
    assert!(x.service.start_authentication().await.is_err());
    assert_eq!(x.service.status().phase, Phase::RateLimited);
    assert_eq!(x.apple.begins.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn does_not_expose_unknown_apple_operation_or_invalid_http_status() {
    let x = fixture(Options {
        begin: Box::new(|| {
            Box::pin(async {
                Err(AppleRemindersError::new(
                    "secret account URL",
                    "secret",
                    Some(900),
                    AppleErrorKind::UnsupportedProtocol,
                ))
            })
        }),
        ..Options::default()
    });
    assert!(x.service.start_authentication().await.is_err());
    assert_eq!(
        x.service.status().diagnostic,
        Some(Diagnostic {
            stage: DiagnosticStage::AppleRequest,
            category: DiagnosticCategory::Protocol,
            http_status: None
        })
    );
}

#[tokio::test]
async fn keeps_incomplete_configuration_disabled_without_storage_or_network_work() {
    let x = fixture(Options::default());
    let disabled = build(
        &RemindersConfiguration {
            storage_key: None,
            ..config()
        },
        x.store.clone(),
        x.apple.clone(),
        Arc::default(),
        None,
        None,
        None,
    );
    assert_eq!(disabled.status().phase, Phase::Disabled);
    disabled.health_check().await;
    assert!(disabled.start_authentication().await.is_err());
    assert_eq!(x.store.reads.load(Ordering::SeqCst), 0);
    assert_eq!(x.apple.begins.load(Ordering::SeqCst), 0);
    assert_eq!(x.apple.verifies.load(Ordering::SeqCst), 0);
}

#[tokio::test(start_paused = true)]
async fn reuses_one_challenge_and_rejects_stale_codes_after_its_deadline() {
    let x = fixture(Options::default());
    let first = x.service.start_authentication().await.unwrap();
    let second = x.service.start_authentication().await.unwrap();
    assert!(first.challenge_id.is_some());
    assert_eq!(second.challenge_id, first.challenge_id);
    assert_eq!(x.apple.begins.load(Ordering::SeqCst), 1);
    tokio::time::sleep(Duration::from_secs(11 * 60)).await;
    assert!(
        x.service
            .submit_code(first.challenge_id.as_deref().unwrap(), "123456")
            .await
            .is_err()
    );
    assert_eq!(x.apple.submits.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn serializes_concurrent_authentication_starts() {
    let x = fixture(Options::default());
    let (first, second) = tokio::join!(
        x.service.start_authentication(),
        x.service.start_authentication()
    );
    assert_eq!(first.unwrap().challenge_id, second.unwrap().challenge_id);
    assert_eq!(x.apple.begins.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn caps_failed_code_attempts_without_issuing_another_apple_request() {
    let x = fixture(Options {
        submit: Box::new(|| {
            Box::pin(async { Err(auth_error(AppleErrorKind::AuthenticationNeeded)) })
        }),
        ..Options::default()
    });
    let status = x.service.start_authentication().await.unwrap();
    for _ in 0..6 {
        assert!(
            x.service
                .submit_code(status.challenge_id.as_deref().unwrap(), "123456")
                .await
                .is_err()
        );
    }
    assert_eq!(x.apple.submits.load(Ordering::SeqCst), 5);
}

#[tokio::test]
async fn invalidates_an_in_memory_challenge_across_service_restart() {
    let x = fixture(Options::default());
    let challenge = x.service.start_authentication().await.unwrap();
    let restarted = x.restart();
    assert!(
        restarted
            .submit_code(challenge.challenge_id.as_deref().unwrap(), "123456")
            .await
            .is_err()
    );
    assert_eq!(x.apple.submits.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn reserves_one_notification_across_repeated_checks_and_restart() {
    let x = fixture(Options::default());
    x.service.verify_access().await;
    x.service.verify_access().await;
    let restarted = x.restart_with(Some(x.notifications.clone()));
    restarted.verify_access().await;
    assert_eq!(x.notifications.load(Ordering::SeqCst), 1);
    assert!(x.state().notified);
    assert!(x.store.writes.load(Ordering::SeqCst) > 0);
}

/// A failed notification reservation is a storage failure. Nothing is
/// delivered and the status reports private storage.
#[tokio::test]
async fn reports_storage_when_the_notification_reservation_cannot_be_saved() {
    let diagnostics = Arc::new(Mutex::new(Vec::new()));
    let x = fixture(Options {
        diagnostics: Some(diagnostics.clone()),
        ..Options::default()
    });
    x.store.fail_writes.store(true, Ordering::SeqCst);
    x.service.health_check().await;
    assert_eq!(x.notifications.load(Ordering::SeqCst), 0);
    let status = x.service.status();
    assert_eq!(status.phase, Phase::TransientOutage);
    let storage = Diagnostic {
        stage: DiagnosticStage::PrivateStorage,
        category: DiagnosticCategory::Storage,
        http_status: None,
    };
    assert_eq!(status.diagnostic, Some(storage));
    assert_eq!(*diagnostics.lock().unwrap(), vec![storage]);
    // The unsaved in-memory reservation still suppresses later prompts.
    let started = x.service.start_authentication().await.unwrap();
    assert_eq!(started.reason, Some(Reason::Mfa));
    assert_eq!(x.notifications.load(Ordering::SeqCst), 0);
}

fn not_found(body: &Value) -> Value {
    json!({"records": [{"recordName": body["records"][0]["recordName"], "serverErrorCode": "NOT_FOUND"}]})
}

#[tokio::test]
async fn leaves_an_uncertain_mutation_reserved_and_never_replays_it() {
    let x = fixture(Options {
        verify: Box::new(|| ready(true)),
        ck: Box::new(|path, body| match path {
            "/records/lookup" => ready(not_found(&body)),
            "/records/modify" => {
                Box::pin(async { Err(auth_error(AppleErrorKind::TransientOutage)) })
            }
            _ => ready(json!({"zones": [{"records": []}]})),
        }),
        ..Options::default()
    });
    assert_eq!(x.service.verify_access().await.phase, Phase::Authenticated);
    let input = ReminderCreateFields {
        list_id: "List/test".into(),
        title: "Milk".into(),
        ..ReminderCreateFields::default()
    };
    assert!(
        x.service
            .create("stable-request-key-1", input.clone())
            .await
            .is_err()
    );
    assert_eq!(
        x.state().operations.values().next().map(|o| o.state),
        Some(OperationState::Reserved)
    );
    let restarted = x.restart();
    restarted.verify_access().await;
    let retry = restarted
        .create("stable-request-key-1", input)
        .await
        .unwrap_err();
    assert_eq!(service_code(&retry), Some(ServiceErrorCode::UncertainWrite));
    assert_eq!(
        x.apple
            .ck_paths()
            .iter()
            .filter(|p| *p == "/records/modify")
            .count(),
        1
    );
}

#[tokio::test]
async fn never_replays_uncertain_recurring_completion_after_service_restart() {
    let reminder_id = "Reminder/12345678";
    let rule_id = "RecurrenceRule/ABCDEFGH";
    let reminder = json!({
        "recordName": reminder_id,
        "recordType": "Reminder",
        "recordChangeTag": "reminder-tag",
        "fields": {
            "TitleDocument": {"type": "STRING", "value": encode_crdt_document("Disposable fixture").unwrap()},
            "List": {"type": "REFERENCE", "value": {"recordName": "List/fixture"}},
            "DueDate": {"type": "TIMESTAMP", "value": 1_793_466_000_000_i64},
            "RecurrenceRuleIDs": {"type": "STRING_LIST", "value": ["ABCDEFGH"]},
        },
    });
    let rule = json!({
        "recordName": rule_id,
        "recordType": "RecurrenceRule",
        "recordChangeTag": "rule-tag",
        "fields": {
            "Reminder": {"type": "REFERENCE", "value": {"recordName": reminder_id}},
            "Frequency": {"type": "INT64", "value": 0},
            "Interval": {"type": "INT64", "value": 1},
        },
    });
    let x = fixture(Options {
        verify: Box::new(|| ready(true)),
        ck: Box::new(move |path, body| match path {
            "/records/lookup" => ready(json!({"records": [
                if body["records"][0]["recordName"] == reminder_id { reminder.clone() } else { rule.clone() }
            ]})),
            "/records/query" if body["query"]["recordType"] == "CompleteRecurringReminder" => {
                Box::pin(async { Err(auth_error(AppleErrorKind::TransientOutage)) })
            }
            "/records/query" => ready(json!({"records": [reminder.clone(), rule.clone()]})),
            _ => ready(json!({"zones": [{"records": []}]})),
        }),
        ..Options::default()
    });
    x.service.verify_access().await;
    let target = || RecurringCompletionTarget {
        id: reminder_id.into(),
        change_tag: "reminder-tag".into(),
        rule_id: rule_id.into(),
        rule_change_tag: "rule-tag".into(),
        time_zone: "America/Vancouver".into(),
    };
    let first = x
        .service
        .complete_recurring("completion-key-123456", target())
        .await
        .unwrap_err();
    assert_eq!(first.code(), "uncertain");
    assert_eq!(
        x.state().operations.values().next().map(|o| o.state),
        Some(OperationState::Reserved)
    );
    let restarted = x.restart();
    restarted.verify_access().await;
    let second = restarted
        .complete_recurring("completion-key-123456", target())
        .await
        .unwrap_err();
    assert_eq!(
        service_code(&second),
        Some(ServiceErrorCode::UncertainWrite)
    );
    let mutations = x
        .apple
        .ck_calls
        .lock()
        .unwrap()
        .iter()
        .filter(|(p, b)| {
            p == "/records/query" && b["query"]["recordType"] == "CompleteRecurringReminder"
        })
        .count();
    assert_eq!(mutations, 1);
}

#[tokio::test]
async fn reserves_recurrence_writes_across_restart_and_never_replays_a_lost_atomic_response() {
    let x = fixture(Options {
        verify: Box::new(|| ready(true)),
        ck: Box::new(|path, body| match path {
            "/records/lookup" => {
                let id = body["records"][0]["recordName"]
                    .as_str()
                    .unwrap()
                    .to_owned();
                ready(json!({"records": [if id.starts_with("RecurrenceRule/") {
                    json!({"recordName": id, "serverErrorCode": "NOT_FOUND"})
                } else {
                    json!({
                        "recordName": id,
                        "recordType": "Reminder",
                        "recordChangeTag": "current",
                        "fields": {
                            "List": {"type": "REFERENCE", "value": {"recordName": "List/test"}},
                            "RecurrenceRuleIDs": {"type": "STRING_LIST", "value": []},
                        },
                    })
                }]}))
            }
            "/records/query" => ready(json!({"records": []})),
            "/records/modify" => {
                Box::pin(async { Err(auth_error(AppleErrorKind::TransientOutage)) })
            }
            _ => ready(json!({"zones": [{"records": []}]})),
        }),
        ..Options::default()
    });
    x.service.verify_access().await;
    let rule = json!({"frequency": "daily", "interval": 1});
    assert!(
        x.service
            .create_recurrence(
                "recurrence-ledger-key",
                "Reminder/12345678",
                "current",
                rule.clone()
            )
            .await
            .is_err()
    );
    let operation = x.state().operations.values().next().cloned().unwrap();
    assert_eq!(operation.state, OperationState::Reserved);
    assert!(operation.record_id.starts_with("RecurrenceRule/"));
    let restarted = x.restart();
    restarted.verify_access().await;
    let replay = restarted
        .create_recurrence(
            "recurrence-ledger-key",
            "Reminder/12345678",
            "current",
            rule,
        )
        .await
        .unwrap_err();
    assert_eq!(
        service_code(&replay),
        Some(ServiceErrorCode::UncertainWrite)
    );
    assert_eq!(
        x.apple
            .ck_paths()
            .iter()
            .filter(|p| *p == "/records/modify")
            .count(),
        1
    );
}

#[tokio::test]
async fn returns_confirmed_list_rename_receipts_without_repeating_writes() {
    let state = Arc::new(Mutex::new(("Original".to_owned(), "old".to_owned())));
    let shared = state.clone();
    let x = fixture(Options {
        verify: Box::new(|| ready(true)),
        ck: Box::new(move |path, _| {
            let mut current = shared.lock().unwrap();
            match path {
                "/records/lookup" => ready(json!({"records": [{
                    "recordName": "List/test",
                    "recordType": "List",
                    "recordChangeTag": current.1,
                    "fields": {"Name": {"type": "STRING", "value": current.0}},
                }]})),
                "/records/modify" => {
                    *current = ("Renamed".into(), "new".into());
                    ready(
                        json!({"records": [{"recordName": "List/test", "recordChangeTag": "new"}]}),
                    )
                }
                _ => ready(json!({"zones": [{"records": []}]})),
            }
        }),
        ..Options::default()
    });
    x.service.verify_access().await;
    let first = x
        .service
        .update_list("list-rename-key-123", "List/test", "old", "Renamed")
        .await
        .unwrap();
    assert_eq!(first["title"], "Renamed");
    assert_eq!(
        x.service
            .update_list("list-rename-key-123", "List/test", "old", "Renamed")
            .await
            .unwrap(),
        first
    );
    assert_eq!(
        x.apple
            .ck_paths()
            .iter()
            .filter(|p| *p == "/records/modify")
            .count(),
        1
    );
    let conflict = x
        .service
        .update_list("list-rename-key-123", "List/test", "old", "Different")
        .await
        .unwrap_err();
    assert_eq!(
        service_code(&conflict),
        Some(ServiceErrorCode::IdempotencyConflict)
    );
}

#[tokio::test]
async fn keeps_a_mutation_reservation_after_its_network_request_is_interrupted() {
    let entered = Arc::new(tokio::sync::Notify::new());
    let signal = entered.clone();
    let tracker = TaskTracker::new();
    let x = fixture(Options {
        verify: Box::new(|| ready(true)),
        tracker: Some(tracker.clone()),
        ck: Box::new(move |path, body| match path {
            "/records/lookup" => ready(not_found(&body)),
            "/records/modify" => {
                signal.notify_one();
                Box::pin(futures::future::pending())
            }
            _ => ready(json!({"zones": [{"records": []}]})),
        }),
        ..Options::default()
    });
    x.service.verify_access().await;
    let service = x.service.clone();
    let request = tokio::spawn(async move {
        service
            .create(
                "interrupted-key-123",
                ReminderCreateFields {
                    list_id: "List/test".into(),
                    title: "Milk".into(),
                    ..ReminderCreateFields::default()
                },
            )
            .await
    });
    entered.notified().await;
    request.abort();
    assert!(request.await.is_err());
    assert_eq!(
        x.state().operations.values().next().map(|o| o.state),
        Some(OperationState::Reserved)
    );
}

#[tokio::test]
async fn derives_stable_record_ids_from_the_idempotency_key() {
    let x = fixture(Options {
        verify: Box::new(|| ready(true)),
        ck: Box::new(|path, body| match path {
            "/records/lookup" => ready(not_found(&body)),
            "/records/modify" => {
                Box::pin(async { Err(auth_error(AppleErrorKind::TransientOutage)) })
            }
            _ => ready(json!({"zones": [{"records": []}]})),
        }),
        ..Options::default()
    });
    x.service.verify_access().await;
    let _ = x
        .service
        .create(
            "stable-request-key-1",
            ReminderCreateFields {
                list_id: "List/test".into(),
                title: "Milk".into(),
                ..ReminderCreateFields::default()
            },
        )
        .await;
    let operation = x.state().operations.into_iter().next().unwrap();
    // sha256("\"stable-request-key-1\"") and the pinned create fingerprint.
    assert_eq!(
        operation.0,
        "b724a43c29ea75aae3c55291c4ecd8c9390a60064e0061a80031f0b9412a5bc4"
    );
    assert_eq!(
        operation.1.record_id,
        "Reminder/B724A43C29EA75AAE3C55291C4ECD8C9"
    );
    assert_eq!(
        operation.1.fingerprint,
        "d762acdfcfdcbf02dd31eb1641a52b991463e2a45a21a68987d1798f2aabc9a1"
    );
}
