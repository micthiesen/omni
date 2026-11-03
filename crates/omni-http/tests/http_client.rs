//! `HttpClient` behavior against local servers: user agent, bounded bodies,
//! redirect rules, timeouts and test overrides. No external network.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use omni_http::{
    HttpClient, HttpConfig, HttpError, HttpOverrides, Method, RedirectRule, USER_AGENT, Url,
};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn client() -> HttpClient {
    HttpClient::new(HttpConfig {
        connect_timeout: Some(Duration::from_secs(2)),
        offline: true,
    })
    .unwrap()
}

fn url(server: &MockServer, p: &str) -> Url {
    Url::parse(&format!("{}{p}", server.uri())).unwrap()
}

#[tokio::test]
async fn sends_the_project_user_agent() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/ua"))
        // wiremock's `header` matcher splits on commas, which the UA contains.
        .and(|request: &wiremock::Request| {
            request
                .headers
                .get("user-agent")
                .and_then(|value| value.to_str().ok())
                == Some(USER_AGENT)
        })
        .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
        .mount(&server)
        .await;
    let response = client()
        .request(Method::GET, url(&server, "/ua"))
        .send_bounded(1024)
        .await
        .unwrap();
    assert_eq!(response.status.as_u16(), 200);
    assert_eq!(&response.body[..], b"ok");
}

#[tokio::test]
async fn rejects_an_oversized_fixed_length_response_before_buffering_it() {
    let server = MockServer::start().await;
    Mock::given(path("/big"))
        .respond_with(ResponseTemplate::new(200).set_body_string("123456"))
        .mount(&server)
        .await;
    let result = client()
        .request(Method::GET, url(&server, "/big"))
        .send_bounded(5)
        .await;
    assert!(matches!(result, Err(HttpError::TooLarge { limit: 5 })));
    let exact = client()
        .request(Method::GET, url(&server, "/big"))
        .send_bounded(6)
        .await
        .unwrap();
    assert_eq!(exact.body.len(), 6);
}

/// Serves one chunked response (no Content-Length) on a raw socket.
async fn chunked_server(chunks: &'static [&'static str]) -> Url {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 4096];
        let _ = socket.read(&mut buf).await;
        let mut response = String::from(
            "HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n",
        );
        for chunk in chunks {
            response.push_str(&format!("{:x}\r\n{chunk}\r\n", chunk.len()));
        }
        response.push_str("0\r\n\r\n");
        let _ = socket.write_all(response.as_bytes()).await;
        let _ = socket.shutdown().await;
    });
    Url::parse(&format!("http://{addr}/stream")).unwrap()
}

#[tokio::test]
async fn rejects_an_oversized_chunked_response_while_streaming() {
    let target = chunked_server(&["123", "456"]).await;
    let result = client().request(Method::GET, target).send_bounded(5).await;
    assert!(matches!(result, Err(HttpError::TooLarge { limit: 5 })));
    let target = chunked_server(&["123", "45"]).await;
    let ok = client()
        .request(Method::GET, target)
        .send_bounded(5)
        .await
        .unwrap();
    assert_eq!(&ok.body[..], b"12345");
}

#[tokio::test]
async fn redirect_rules() {
    let server = MockServer::start().await;
    Mock::given(path("/from"))
        .respond_with(ResponseTemplate::new(302).insert_header("location", "/to"))
        .mount(&server)
        .await;
    Mock::given(path("/to"))
        .respond_with(ResponseTemplate::new(200).set_body_string("landed"))
        .mount(&server)
        .await;
    Mock::given(path("/loop"))
        .respond_with(ResponseTemplate::new(301).insert_header("location", "/loop"))
        .mount(&server)
        .await;

    let default = client()
        .request(Method::GET, url(&server, "/from"))
        .send_bounded(1024)
        .await;
    assert!(matches!(
        default,
        Err(HttpError::Status { status: 302, .. })
    ));

    let raw = client()
        .request(Method::GET, url(&server, "/from"))
        .redirect(RedirectRule::None)
        .send_bounded(1024)
        .await
        .unwrap();
    assert_eq!(raw.status.as_u16(), 302);

    let followed = client()
        .request(Method::GET, url(&server, "/from"))
        .redirect(RedirectRule::Follow(3))
        .send_bounded(1024)
        .await
        .unwrap();
    assert_eq!(&followed.body[..], b"landed");
    assert_eq!(followed.final_url.path(), "/to");

    let looping = client()
        .request(Method::GET, url(&server, "/loop"))
        .redirect(RedirectRule::Follow(3))
        .send_bounded(1024)
        .await;
    assert!(matches!(looping, Err(HttpError::Blocked(_))));
}

#[tokio::test]
async fn network_errors_name_only_the_origin() {
    // A port that was just released refuses the connection.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let target = format!(
        "http://user:pw-secret-1@127.0.0.1:{port}/3/device/path-secret-2?api_key=query-secret-3"
    );
    let error = client()
        .request(Method::GET, Url::parse(&target).unwrap())
        .send_bounded(1024)
        .await
        .unwrap_err();
    let HttpError::Network(text) = &error else {
        panic!("expected a network error, got {error:?}");
    };
    for secret in ["pw-secret-1", "path-secret-2", "query-secret-3", "user:"] {
        assert!(!text.contains(secret), "{secret} in {text}");
    }
    assert!(text.contains(&format!("http://127.0.0.1:{port}")), "{text}");
}

#[tokio::test]
async fn cross_origin_redirects_forward_no_credential_headers() {
    let first = MockServer::start().await;
    let second = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/same"))
        .respond_with(ResponseTemplate::new(302).insert_header("location", "/kept"))
        .mount(&first)
        .await;
    Mock::given(method("GET"))
        .and(path("/kept"))
        .and(|request: &wiremock::Request| request.headers.contains_key("x-api-key"))
        .respond_with(ResponseTemplate::new(200).set_body_string("kept"))
        .mount(&first)
        .await;
    Mock::given(method("GET"))
        .and(path("/away"))
        .respond_with(
            ResponseTemplate::new(302)
                .insert_header("location", format!("{}/landed", second.uri())),
        )
        .mount(&first)
        .await;
    Mock::given(method("GET"))
        .and(path("/landed"))
        .respond_with(ResponseTemplate::new(200).set_body_string("landed"))
        .mount(&second)
        .await;

    let send = |p: &'static str| {
        client()
            .request(Method::GET, url(&first, p))
            .header("x-api-key", "arr-key")
            .header("x-plex-token", "plex-token")
            .header("authorization", "Bearer token")
            .header("accept", "application/json")
            .redirect(RedirectRule::Follow(3))
            .send_bounded(1024)
    };
    let same = send("/same").await.unwrap();
    assert_eq!(&same.body[..], b"kept");

    let away = send("/away").await.unwrap();
    assert_eq!(&away.body[..], b"landed");
    let received = second.received_requests().await.unwrap();
    let headers = &received[0].headers;
    for name in ["x-api-key", "x-plex-token", "authorization"] {
        assert!(!headers.contains_key(name), "{name} crossed origins");
    }
    assert_eq!(headers.get("accept").unwrap(), "application/json");
    assert_eq!(headers.get("user-agent").unwrap(), USER_AGENT);
}

#[tokio::test]
async fn see_other_turns_a_post_into_a_bodiless_get() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/submit"))
        .respond_with(ResponseTemplate::new(303).insert_header("location", "/done"))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/done"))
        .respond_with(ResponseTemplate::new(200).set_body_string("done"))
        .mount(&server)
        .await;
    let response = client()
        .request(Method::POST, url(&server, "/submit"))
        .json(&serde_json::json!({ "a": 1 }))
        .redirect(RedirectRule::Follow(1))
        .send_bounded(1024)
        .await
        .unwrap();
    assert_eq!(&response.body[..], b"done");
}

#[tokio::test]
async fn times_out_across_the_whole_request() {
    let server = MockServer::start().await;
    Mock::given(path("/slow"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(5)))
        .mount(&server)
        .await;
    let result = client()
        .request(Method::GET, url(&server, "/slow"))
        .timeout(Duration::from_millis(100))
        .send_bounded(1024)
        .await;
    assert!(matches!(result, Err(HttpError::Timeout)));
}

#[tokio::test]
async fn json_bounded_maps_non_2xx_to_status_errors() {
    let server = MockServer::start().await;
    Mock::given(path("/ok"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "v": 1 })))
        .mount(&server)
        .await;
    Mock::given(path("/bad"))
        .respond_with(ResponseTemplate::new(503).set_body_string("down"))
        .mount(&server)
        .await;
    let value: serde_json::Value = client()
        .request(Method::GET, url(&server, "/ok"))
        .json_bounded(1024)
        .await
        .unwrap();
    assert_eq!(value, serde_json::json!({ "v": 1 }));
    let error = client()
        .request(Method::GET, url(&server, "/bad"))
        .json_bounded::<serde_json::Value>(1024)
        .await
        .unwrap_err();
    assert!(matches!(&error, HttpError::Status { status: 503, body } if body == "down"));
    assert!(error.is_transient());
}

#[tokio::test]
async fn offline_clients_refuse_dns_and_overrides_reach_the_mock() {
    let server = MockServer::start().await;
    Mock::given(path("/1/messages.json"))
        .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
        .mount(&server)
        .await;
    let offline = client()
        .request(
            Method::GET,
            Url::parse("https://api.pushover.net/1/messages.json").unwrap(),
        )
        .send_bounded(1024)
        .await;
    assert!(matches!(offline, Err(HttpError::Network(_))));
    let rewritten = client()
        .with_overrides(HttpOverrides {
            rewrites: vec![(
                Url::parse("https://api.pushover.net").unwrap(),
                Url::parse(&server.uri()).unwrap(),
            )],
        })
        .request(
            Method::POST,
            Url::parse("https://api.pushover.net/1/messages.json").unwrap(),
        )
        .send_bounded(1024)
        .await
        .unwrap();
    assert_eq!(rewritten.status.as_u16(), 200);
}
