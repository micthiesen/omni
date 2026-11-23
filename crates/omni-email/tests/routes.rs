//! The email REST routes through the subsystem router.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use omni_api::email::paths;
use omni_email::activity::{self, EmailActivityOutcome, EmailPipelineName, LlmCost, NewActivity};
use omni_email::activity_logs::{self, EmailActivityLogData};
use omni_email::retry;
use omni_store::LogLine;
use omni_testkit::TestApp;
use serde_json::{Value, json};
use tower::ServiceExt as _;

use common::{FakeReader, email, fn_handler, handlers};

async fn app() -> (TestApp, axum::Router) {
    let app = TestApp::new().await;
    let subsystem = omni_email::subsystem(&app.ctx, 0).unwrap();
    let router = app.router(&subsystem);
    (app, router)
}

async fn send(router: &axum::Router, method: Method, path: &str) -> (StatusCode, Value) {
    let request = Request::builder()
        .method(method)
        .uri(path)
        .header(header::HOST, "localhost")
        .body(Body::empty())
        .unwrap();
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn seed(app: &TestApp, id: &str, outcome: EmailActivityOutcome) {
    let mut fetched = email(id);
    fetched.received_at = "2026-01-01T00:00:00.000Z".to_owned();
    activity::record(
        &app.ctx.store,
        NewActivity {
            cost_cents: LlmCost::Unpriced,
            detail: Some("blacklisted sender".to_owned()),
            ..NewActivity::new(EmailPipelineName::ParcelTracker, &fetched, outcome)
        },
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn lists_activity_with_explicit_nulls_and_validates_the_pipeline() {
    let (app, router) = app().await;
    seed(&app, "<a@x>", EmailActivityOutcome::Filtered).await;
    let (status, body) = app.get_json(&router, "/api/email-activity").await;
    assert_eq!(status, StatusCode::OK);
    let row = &body["activities"][0];
    assert_eq!(row["activityId"], "ParcelTracker#<a@x>");
    assert_eq!(row["receivedAt"], 1_767_225_600_000_i64);
    assert_eq!(row["outcome"], "filtered");
    assert_eq!(row["admitReason"], Value::Null);
    assert_eq!(row["admitTier"], Value::Null);
    assert_eq!(row["costCents"], Value::Null);
    assert_eq!(row["items"], json!([]));

    let (status, body) = app
        .get_json(&router, "/api/email-activity?pipeline=Nope")
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body, json!({"error": "Unknown pipeline"}));
    let (_, body) = app
        .get_json(
            &router,
            "/api/email-activity?pipeline=CalendarEvents&limit=abc",
        )
        .await;
    assert_eq!(body["activities"], json!([]));
}

#[tokio::test]
async fn serves_activity_logs_and_404s_unknown_activity() {
    let (app, router) = app().await;
    seed(&app, "m1", EmailActivityOutcome::Processed).await;
    activity_logs::save(
        &app.ctx.store,
        EmailActivityLogData {
            activity_id: "ParcelTracker#m1".to_owned(),
            lines: vec![LogLine {
                t: 5,
                level: omni_core::LogLevel::Warn,
                logger: "Main:ParcelTracker".to_owned(),
                msg: "hello".to_owned(),
            }],
            dropped: 2,
        },
    )
    .await
    .unwrap();
    let (status, body) = app
        .get_json(&router, &paths::email_activity_logs("ParcelTracker#m1"))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["lines"],
        json!([{"t": 5, "level": "warn", "logger": "Main:ParcelTracker", "msg": "hello"}])
    );
    assert_eq!(body["dropped"], 2);
    let (status, body) = app
        .get_json(&router, &paths::email_activity_logs("ParcelTracker#none"))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body, json!({"error": "Unknown activity"}));
}

#[tokio::test]
async fn reprocess_needs_active_pipelines_and_clears_the_retry() {
    let (app, router) = app().await;
    seed(&app, "m1", EmailActivityOutcome::Error).await;
    retry::enqueue(&app.ctx.store, "ParcelTracker", "m1", "503")
        .await
        .unwrap();
    let path = paths::email_activity_reprocess("ParcelTracker#m1");
    let (status, body) = app.post_json(&router, &path, &json!({})).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body, json!({"error": "Email pipelines are not active"}));

    app.ctx
        .ports
        .set_email_reader(FakeReader::new(|_| Ok(None)))
        .ok()
        .unwrap();
    app.ctx
        .ports
        .set_email_retry_handlers(handlers(vec![(
            "ParcelTracker",
            fn_handler("ParcelTracker", |_| async {
                Ok::<(), omni_core::email::HandlerError>(())
            }) as std::sync::Arc<dyn omni_core::email::EmailHandler>,
        )]))
        .ok()
        .unwrap();
    let (status, body) = app.post_json(&router, &path, &json!({})).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        body,
        json!({"error": "Email no longer exists in the mailbox"})
    );
    assert!(
        retry::get(&app.ctx.store, "ParcelTracker#m1")
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn manages_rules_with_builtin_and_merge_statuses() {
    let (app, router) = app().await;
    let (status, body) = app
        .post_json(
            &router,
            paths::EMAIL_RULES,
            &json!({"pattern": "Shop <orders@shop.com>", "scope": "parcel", "verdict": "allow"}),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["status"], "created");
    assert_eq!(body["rule"]["ruleId"], "parcel:orders@shop.com");

    let (status, body) = app
        .post_json(
            &router,
            paths::EMAIL_RULES,
            &json!({"pattern": "orders@shop.com", "scope": "calendar", "verdict": "allow"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "merged");

    let (status, body) = app
        .post_json(
            &router,
            paths::EMAIL_RULES,
            &json!({"pattern": "support@npmjs.com", "scope": "both", "verdict": "block"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        json!({"status": "builtin", "message": "Already blocked by a built-in list"})
    );

    let (status, body) = app
        .post_json(
            &router,
            paths::EMAIL_RULES,
            &json!({"pattern": "", "scope": "x"}),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        body,
        json!({"error": "pattern, scope, and verdict are required"})
    );

    let (_, body) = app.get_json(&router, paths::EMAIL_RULES).await;
    assert_eq!(body["rules"].as_array().unwrap().len(), 1);
    assert_eq!(body["builtin"]["parcel"]["blocked"][0], "@amazon.");
    assert_eq!(body["builtin"]["calendar"]["autoPass"][0], "@united.com");

    let (status, body) = send(
        &router,
        Method::DELETE,
        &paths::email_rule("both:orders@shop.com"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({"deleted": true}));
    let (status, body) = send(
        &router,
        Method::DELETE,
        &paths::email_rule("both:orders@shop.com"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body, json!({"error": "Unknown rule"}));
}

#[tokio::test]
async fn records_and_clears_feedback() {
    let (app, router) = app().await;
    seed(&app, "m1", EmailActivityOutcome::Processed).await;
    let path = paths::email_activity_feedback("ParcelTracker#m1");
    let (status, body) = app.post_json(&router, &path, &json!({"note": "x"})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        body,
        json!({"error": "A verdict (not_relevant | missed | null) is required"})
    );

    let (status, body) = app
        .post_json(
            &router,
            &path,
            &json!({"verdict": "not_relevant", "note": "promo"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["feedback"]["activityId"], "ParcelTracker#m1");
    assert_eq!(body["feedback"]["note"], "promo");

    let (_, body) = app.get_json(&router, paths::EMAIL_FEEDBACK).await;
    assert_eq!(body["feedback"].as_array().unwrap().len(), 1);

    let (status, body) = app
        .post_json(&router, &path, &json!({"verdict": null}))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({"feedback": null}));
    let (_, body) = app.get_json(&router, paths::EMAIL_FEEDBACK).await;
    assert_eq!(body, json!({"feedback": []}));

    let (status, _) = app
        .post_json(
            &router,
            &paths::email_activity_feedback("ParcelTracker#none"),
            &json!({"verdict": null}),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
