//! Trusted browsers: a browser becomes trusted only when the owner approves
//! it from the phone push by typing the code the browser shows, or opens a
//! one-time link; headers alone never make a browser trusted.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use omni_testkit::TestApp;
use serde_json::Value;
use tower::ServiceExt as _;

const SAFARI: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.0 Safari/605.1.15";

struct Reply {
    status: StatusCode,
    cookie: Option<String>,
    text: String,
}

impl Reply {
    fn json(&self) -> Value {
        serde_json::from_str(&self.text).unwrap_or(Value::Null)
    }
}

async fn send(router: &axum::Router, request: Request<Body>) -> Reply {
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let cookie = response
        .headers()
        .get(header::SET_COOKIE)
        .map(|v| v.to_str().unwrap().to_owned());
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    Reply {
        status,
        cookie,
        text: String::from_utf8_lossy(&bytes).into_owned(),
    }
}

fn post(path: &str, body: Option<&Value>, cookie: Option<&str>) -> Request<Body> {
    let mut builder = Request::post(path)
        .header(header::HOST, "omni.boris")
        .header(header::ORIGIN, "http://omni.boris")
        .header(header::USER_AGENT, SAFARI)
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(cookie) = cookie {
        builder = builder.header(header::COOKIE, cookie);
    }
    builder
        .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
        .unwrap()
}

fn form(path: &str, body: &str) -> Request<Body> {
    Request::post(path)
        .header(header::HOST, "omni.boris")
        .header(header::ORIGIN, "http://omni.boris")
        .header(header::USER_AGENT, SAFARI)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body.to_owned()))
        .unwrap()
}

fn get(path: &str, cookie: Option<&str>) -> Request<Body> {
    let mut builder = Request::get(path).header(header::HOST, "omni.boris");
    if let Some(cookie) = cookie {
        builder = builder.header(header::COOKIE, cookie);
    }
    builder.body(Body::empty()).unwrap()
}

/// The approval path from the push's link.
async fn pushed_path(app: &TestApp) -> String {
    for _ in 0..200 {
        if let Some(push) = app.pushes.all().into_iter().find(|p| {
            p.message
                .url
                .as_deref()
                .is_some_and(|u| u.contains("/trust/"))
        }) {
            let url = push.message.url.unwrap();
            assert!(
                !push.message.message.contains("-"),
                "the push never carries the code: {}",
                push.message.message
            );
            return url[url.find("/trust/").unwrap()..].to_owned();
        }
        tokio::task::yield_now().await;
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    panic!("no approval push was sent");
}

/// The `name=value` part of a `Set-Cookie`.
fn cookie_pair(set_cookie: &str) -> String {
    set_cookie.split(';').next().unwrap().to_owned()
}

#[tokio::test]
async fn a_browser_is_trusted_only_after_the_owner_types_its_code() {
    let app = TestApp::new().await;
    let router = omni_runtime::owner::router(app.ctx.owner.clone());

    let status = send(&router, get("/api/owner", None)).await;
    assert_eq!(status.json()["trusted"], false);
    assert_eq!(status.json()["browsers"], serde_json::json!([]));

    let started = send(&router, post("/api/owner/pairings", None, None)).await;
    assert_eq!(started.status, StatusCode::CREATED, "{}", started.text);
    let started = started.json();
    let id = started["id"].as_str().unwrap().to_owned();
    let code = started["code"].as_str().unwrap().to_owned();
    let claim = serde_json::json!({"claim": started["claim"]});
    assert_eq!(code.len(), 9, "{code}");

    // Another request within a minute is refused (anyone on the LAN can ask).
    let again = send(&router, post("/api/owner/pairings", None, None)).await;
    assert_eq!(again.status, StatusCode::TOO_MANY_REQUESTS);

    let claim_path = format!("/api/owner/pairings/{id}/claim");
    let pending = send(&router, post(&claim_path, Some(&claim), None)).await;
    assert_eq!(pending.json()["state"], "pending");
    assert!(pending.cookie.is_none());

    // A wrong claim secret learns nothing.
    let forged = serde_json::json!({"claim": "forged"});
    let wrong = send(&router, post(&claim_path, Some(&forged), None)).await;
    assert_eq!(wrong.json()["state"], "expired");

    let approval = pushed_path(&app).await;
    let page = send(&router, get(&approval, None)).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.text.contains("Safari on macOS"), "{}", page.text);
    assert!(
        !page.text.contains(&code),
        "the page must not show the code"
    );

    // A cross-origin form post is refused.
    let mut cross = form(&approval, "action=approve");
    cross
        .headers_mut()
        .insert(header::ORIGIN, "http://evil.example".parse().unwrap());
    assert_eq!(send(&router, cross).await.status, StatusCode::FORBIDDEN);

    let wrong_code = send(&router, form(&approval, "action=approve&code=AAAA-AAAA")).await;
    assert_eq!(wrong_code.status, StatusCode::BAD_REQUEST);
    assert!(
        wrong_code.text.contains("4 tries left"),
        "{}",
        wrong_code.text
    );

    let typed = code.to_lowercase().replace('-', " ");
    let approved = send(
        &router,
        form(&approval, &format!("action=approve&code={typed}")),
    )
    .await;
    assert_eq!(approved.status, StatusCode::OK, "{}", approved.text);
    assert!(
        approved.cookie.is_none(),
        "the phone is not the one trusted"
    );

    let trusted = send(&router, post(&claim_path, Some(&claim), None)).await;
    assert_eq!(trusted.json()["state"], "trusted");
    let set_cookie = trusted.cookie.expect("the claim sets the cookie");
    assert!(set_cookie.contains("HttpOnly") && set_cookie.contains("SameSite=Strict"));
    let cookie = cookie_pair(&set_cookie);

    // The cookie works once and the pairing cannot be claimed again.
    let replay = send(&router, post(&claim_path, Some(&claim), None)).await;
    assert_eq!(replay.json()["state"], "expired");
    assert!(replay.cookie.is_none());
    let gone = send(&router, get(&approval, None)).await;
    assert_eq!(gone.status, StatusCode::NOT_FOUND);

    let status = send(&router, get("/api/owner", Some(&cookie))).await.json();
    assert_eq!(status["trusted"], true);
    assert_eq!(status["browsers"][0]["label"], "Safari on macOS");
    assert_eq!(status["browsers"][0]["current"], true);

    // Removing a browser needs a trusted one; afterwards its cookie is void.
    let browser = status["browsers"][0]["id"].as_str().unwrap().to_owned();
    let mut untrusted = Request::delete(format!("/api/owner/browsers/{browser}"))
        .header(header::HOST, "omni.boris")
        .header(header::ORIGIN, "http://omni.boris")
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&router, untrusted).await.status, StatusCode::FORBIDDEN);
    untrusted = Request::delete(format!("/api/owner/browsers/{browser}"))
        .header(header::HOST, "omni.boris")
        .header(header::ORIGIN, "http://omni.boris")
        .header(header::COOKIE, cookie.as_str())
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        send(&router, untrusted).await.status,
        StatusCode::NO_CONTENT
    );
    let status = send(&router, get("/api/owner", Some(&cookie))).await.json();
    assert_eq!(status["trusted"], false);
}

#[tokio::test]
async fn five_wrong_codes_deny_the_request() {
    let app = TestApp::new().await;
    let router = omni_runtime::owner::router(app.ctx.owner.clone());
    let started = send(&router, post("/api/owner/pairings", None, None))
        .await
        .json();
    let id = started["id"].as_str().unwrap().to_owned();
    let claim = serde_json::json!({"claim": started["claim"]});
    let approval = pushed_path(&app).await;
    for _ in 0..4 {
        let reply = send(&router, form(&approval, "action=approve&code=ZZZZ-ZZZZ")).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    }
    let last = send(&router, form(&approval, "action=approve&code=ZZZZ-ZZZZ")).await;
    assert!(last.text.contains("Denied"), "{}", last.text);
    let claimed = send(
        &router,
        post(
            &format!("/api/owner/pairings/{id}/claim"),
            Some(&claim),
            None,
        ),
    )
    .await;
    assert_eq!(claimed.json()["state"], "denied");
    assert!(claimed.cookie.is_none());
}

#[tokio::test]
async fn a_one_time_link_trusts_the_browser_that_opens_it() {
    let app = TestApp::new().await;
    let router = omni_runtime::owner::router(app.ctx.owner.clone());
    let url = app.ctx.owner.create_link("CLI").await.unwrap();
    let path = url[url.find("/trust/").unwrap()..].to_owned();

    let page = send(&router, get(&path, None)).await;
    assert!(page.text.contains("Trust this browser"), "{}", page.text);
    let trusted = send(&router, form(&path, "action=approve")).await;
    assert_eq!(trusted.status, StatusCode::OK);
    let cookie = cookie_pair(&trusted.cookie.expect("the opener gets the cookie"));
    let status = send(&router, get("/api/owner", Some(&cookie))).await.json();
    assert_eq!(status["trusted"], true);
    assert_eq!(
        status["browsers"][0]["label"],
        "Safari on macOS (owner-link)"
    );

    // The link works once.
    let again = send(&router, form(&path, "action=approve")).await;
    assert_eq!(again.status, StatusCode::NOT_FOUND);
    assert!(again.cookie.is_none());
}
