//! Shared email triage, plus the model-backed classifier with a scripted
//! `FakeModels` triage response.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use omni_ai::{GenerateResponse, ModelRole};
use omni_email::activity::{EmailPipelineName, LlmCost};
use omni_email::feedback::{self, EmailFeedbackVerdict, NewFeedback};
use omni_email::triage::{
    Classified, EmailTriage, MAX_TRIAGE_CACHE_ENTRIES, TriageClassifier, TriageEmail, TriageError,
    TriageVerdict, triage_prompt,
};
use tokio::sync::oneshot;

use common::{NOW, store_at};

fn verdict() -> TriageVerdict {
    TriageVerdict {
        parcel: true,
        calendar: false,
        reason: "tracking".to_owned(),
    }
}

fn make_email(id: &str) -> TriageEmail {
    TriageEmail {
        id: id.to_owned(),
        subject: format!("Subject {id}"),
        from: "orders@shop.com".to_owned(),
        text_body: "body text".to_owned(),
        links: Vec::new(),
    }
}

type Script =
    Box<dyn Fn(&TriageEmail) -> BoxFuture<'static, Result<Classified, TriageError>> + Send + Sync>;

struct ScriptedClassifier {
    calls: Mutex<Vec<String>>,
    script: Script,
}

impl ScriptedClassifier {
    fn new(script: Script) -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::new(Vec::new()),
            script,
        })
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

impl TriageClassifier for ScriptedClassifier {
    fn classify(&self, email: TriageEmail) -> BoxFuture<'static, Result<Classified, TriageError>> {
        self.calls.lock().unwrap().push(email.id.clone());
        (self.script)(&email)
    }
}

fn always(verdict: TriageVerdict) -> Script {
    Box::new(move |_| {
        let verdict = verdict.clone();
        Box::pin(async move {
            Ok(Classified {
                verdict,
                cost: None,
            })
        })
    })
}

#[tokio::test]
async fn classifies_through_the_typed_api() {
    let classifier = ScriptedClassifier::new(always(verdict()));
    let triage = EmailTriage::new(classifier.clone());
    assert_eq!(
        triage.classify(&make_email("effect")).await.unwrap(),
        verdict()
    );
    assert_eq!(classifier.calls().len(), 1);
}

#[tokio::test]
async fn shares_one_in_flight_call_between_concurrent_classifies_of_the_same_email() {
    let classifier = ScriptedClassifier::new(Box::new(|_| {
        Box::pin(async {
            tokio::task::yield_now().await;
            Ok(Classified {
                verdict: verdict(),
                cost: None,
            })
        })
    }));
    let triage = EmailTriage::new(classifier.clone());
    let email = make_email("e1");
    let (a, b) = tokio::join!(triage.classify(&email), triage.classify(&email));
    assert_eq!(a.unwrap(), verdict());
    assert_eq!(b.unwrap(), verdict());
    assert_eq!(classifier.calls().len(), 1);
}

#[tokio::test]
async fn classifies_distinct_emails_separately() {
    let classifier = ScriptedClassifier::new(always(verdict()));
    let triage = EmailTriage::new(classifier.clone());
    triage.classify(&make_email("e1")).await.unwrap();
    triage.classify(&make_email("e2")).await.unwrap();
    assert_eq!(classifier.calls().len(), 2);
}

#[tokio::test]
async fn does_not_cache_failures_a_later_classify_retries_and_can_succeed() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let counter = attempts.clone();
    let classifier = ScriptedClassifier::new(Box::new(move |email| {
        let first = counter.fetch_add(1, Ordering::SeqCst) == 0;
        let id = email.id.clone();
        Box::pin(async move {
            if first {
                Err(TriageError {
                    email_id: id,
                    message: "model down".to_owned(),
                })
            } else {
                Ok(Classified {
                    verdict: verdict(),
                    cost: None,
                })
            }
        })
    }));
    let triage = EmailTriage::new(classifier.clone());
    let logs = omni_testkit::capture_logs();
    let email = make_email("e1");
    let error = triage.classify(&email).await.unwrap_err();
    assert_eq!(error.message, "model down");
    assert!(
        logs.events()
            .iter()
            .any(|e| e.level == tracing::Level::WARN)
    );
    assert_eq!(triage.classify(&email).await.unwrap(), verdict());
    assert_eq!(classifier.calls().len(), 2);
}

#[tokio::test]
async fn evicts_the_oldest_entry_once_the_cache_cap_is_exceeded() {
    let classifier = ScriptedClassifier::new(always(verdict()));
    let triage = EmailTriage::new(classifier.clone());
    triage.classify(&make_email("first")).await.unwrap();
    for i in 0..MAX_TRIAGE_CACHE_ENTRIES {
        triage
            .classify(&make_email(&format!("filler-{i}")))
            .await
            .unwrap();
    }
    // "first" was evicted, so classifying it again calls the model again.
    triage.classify(&make_email("first")).await.unwrap();
    assert_eq!(classifier.calls().len(), MAX_TRIAGE_CACHE_ENTRIES + 2);
}

#[tokio::test]
async fn never_evicts_an_in_flight_entry_when_the_cache_cap_is_exceeded() {
    let (resolve_first, first) = oneshot::channel::<TriageVerdict>();
    let first = Arc::new(Mutex::new(Some(first)));
    let classifier = ScriptedClassifier::new(Box::new(move |email| {
        if email.id == "first" {
            let receiver = first.lock().unwrap().take();
            Box::pin(async move {
                let verdict = receiver.unwrap().await.unwrap();
                Ok(Classified {
                    verdict,
                    cost: None,
                })
            })
        } else {
            Box::pin(async {
                Ok(Classified {
                    verdict: verdict(),
                    cost: None,
                })
            })
        }
    }));
    let triage = EmailTriage::new(classifier.clone());
    let first_email = make_email("first");
    let pending = triage.classify(&first_email);
    tokio::pin!(pending);
    // Start the first call so it is in flight.
    assert!(futures::poll!(pending.as_mut()).is_pending());
    for i in 0..MAX_TRIAGE_CACHE_ENTRIES {
        triage
            .classify(&make_email(&format!("filler-{i}")))
            .await
            .unwrap();
    }
    let same_pending = triage.classify(&first_email);
    assert_eq!(
        classifier
            .calls()
            .iter()
            .filter(|id| *id == "first")
            .count(),
        1
    );
    resolve_first.send(verdict()).unwrap();
    let (a, b) = tokio::join!(pending, same_pending);
    assert_eq!(a.unwrap(), verdict());
    assert_eq!(b.unwrap(), verdict());
}

#[tokio::test]
async fn includes_sender_subject_and_a_truncated_body() {
    let (store, _clock) = store_at(NOW).await;
    let email = TriageEmail {
        text_body: format!("{}TAIL", "x".repeat(1500)),
        ..make_email("e1")
    };
    let prompt = triage_prompt(&store.store, &email).await.unwrap();
    assert!(prompt.contains("From: orders@shop.com"));
    assert!(prompt.contains("Subject: Subject e1"));
    assert!(prompt.contains(&"x".repeat(1500)));
    assert!(!prompt.contains("TAIL"));
}

#[tokio::test]
async fn caps_links_at_five_and_omits_the_section_when_there_are_none() {
    let (store, _clock) = store_at(NOW).await;
    let links: Vec<String> = (0..7).map(|i| format!("https://l.test/{i}")).collect();
    let prompt = triage_prompt(
        &store.store,
        &TriageEmail {
            links,
            ..make_email("e1")
        },
    )
    .await
    .unwrap();
    assert!(prompt.contains("https://l.test/4"));
    assert!(!prompt.contains("https://l.test/5"));
    assert!(
        !triage_prompt(&store.store, &make_email("e2"))
            .await
            .unwrap()
            .contains("Links:")
    );
}

#[tokio::test]
async fn appends_user_correction_digests_only_when_feedback_exists() {
    let (store, _clock) = store_at(NOW).await;
    let heading = "Recent user corrections — follow these";
    assert!(
        !triage_prompt(&store.store, &make_email("e1"))
            .await
            .unwrap()
            .contains(heading)
    );
    feedback::record(
        &store.store,
        NewFeedback {
            pipeline: EmailPipelineName::ParcelTracker,
            email_id: "fb1".to_owned(),
            subject: "npm package published".to_owned(),
            from: "support@npmjs.com".to_owned(),
            verdict: EmailFeedbackVerdict::NotRelevant,
            note: None,
        },
    )
    .await
    .unwrap();
    let prompt = triage_prompt(&store.store, &make_email("e2"))
        .await
        .unwrap();
    assert!(prompt.contains(heading));
    assert!(
        prompt.contains(
            "- \"npm package published\" from support@npmjs.com: user marked NOT relevant"
        )
    );
}

#[tokio::test]
async fn the_model_classifier_decodes_the_verdict_and_records_its_cost() {
    let app = omni_testkit::TestApp::new().await;
    app.ai.script(
        ModelRole::Triage,
        vec![GenerateResponse {
            usage: omni_ai::Usage {
                input_tokens: 1_000,
                output_tokens: 100,
                ..omni_ai::Usage::default()
            },
            ..GenerateResponse::text(r#"{"parcel":true,"calendar":false,"reason":"tracking"}"#)
        }],
    );
    let triage = EmailTriage::with_model(
        app.ctx.ai.clone(),
        app.ctx.config.clone(),
        app.ctx.store.clone(),
    );
    assert_eq!(triage.classify(&make_email("m1")).await.unwrap(), verdict());
    assert!(matches!(triage.triage_cost_cents("m1"), LlmCost::Cents(cents) if cents > 0.0));
    assert_eq!(triage.triage_cost_cents("unknown"), LlmCost::Unpriced);
    let requests = app.ai.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].0, Some(ModelRole::Triage));
    assert!(requests[0].1.output.is_some());
}
