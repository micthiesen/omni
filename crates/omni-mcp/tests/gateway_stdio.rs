//! Bundled stdio servers: a cleared environment with only the declared
//! variables, tool calls over newline-delimited JSON-RPC, a restart after
//! the process dies, and stderr diagnostics without secrets.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;
mod gateway_support;

use std::path::PathBuf;

use gateway_support::{Fixture, Options};
use omni_api::gateway::{
    AuthKind, CredentialsUpdate, IntegrationCreate, IntegrationStatus, SecretPair, TransportInput,
};
use omni_mcp::gateway::bundled::BundledRegistry;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

const SERVER: &str = r#"
import json, os, sys

def send(message):
    sys.stdout.write(json.dumps(message) + "\n")
    sys.stdout.flush()

for line in sys.stdin:
    message = json.loads(line)
    method = message.get("method")
    ident = message.get("id")
    if ident is None:
        continue
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": ident, "result": {"protocolVersion": "2025-06-18", "capabilities": {"tools": {}}, "serverInfo": {"name": "fake-stdio", "version": "0.1"}}})
    elif method == "tools/list":
        tools = [{"name": n, "inputSchema": {"type": "object"}} for n in ("env", "echo", "crash")]
        send({"jsonrpc": "2.0", "id": ident, "result": {"tools": tools}})
    elif method == "tools/call":
        name = message["params"]["name"]
        if name == "crash":
            sys.stderr.write("fatal: key " + os.environ.get("FAKE_KEY", "") + " rejected\n")
            sys.stderr.flush()
            sys.exit(3)
        if name == "env":
            text = json.dumps(sorted(os.environ.keys()))
        else:
            text = json.dumps(message["params"].get("arguments"))
        send({"jsonrpc": "2.0", "id": ident, "result": {"content": [{"type": "text", "text": text}], "structuredContent": {"pid": os.getpid()}}})
    else:
        send({"jsonrpc": "2.0", "id": ident, "error": {"code": -32601, "message": "nope"}})
"#;

async fn stdio_fixture() -> (Fixture, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("server.py");
    std::fs::write(&script, SERVER).unwrap();
    let bundled = BundledRegistry::custom([(
        "fake".to_owned(),
        vec!["FAKE_KEY".to_owned(), "FAKE_ADDR".to_owned()],
        PathBuf::from("python3"),
        vec!["-I".to_owned(), script.to_string_lossy().into_owned()],
    )]);
    let fixture = Fixture::with(Options {
        bundled,
        ..Options::default()
    })
    .await;
    fixture
        .gateway
        .create(IntegrationCreate {
            id: "keep".into(),
            name: "Fake stdio".into(),
            transport: TransportInput::Stdio {
                server: "fake".into(),
            },
            auth: AuthKind::Env,
            timeout_seconds: Some(10),
            enabled: Some(true),
        })
        .await
        .unwrap();
    (fixture, dir)
}

fn env(pairs: &[(&str, &str)]) -> CredentialsUpdate {
    CredentialsUpdate {
        headers: Vec::new(),
        env: pairs
            .iter()
            .map(|(name, value)| SecretPair {
                name: (*name).into(),
                value: (*value).into(),
            })
            .collect(),
    }
}

async fn call(fixture: &Fixture, name: &str) -> serde_json::Map<String, Value> {
    fixture
        .gateway
        .call_tool(name, json!({"a": 1}), CancellationToken::new())
        .await
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn a_bundled_server_sees_only_its_declared_variables() {
    let (fixture, _dir) = stdio_fixture().await;
    assert!(!fixture.refresh("keep").await.ok, "no variables stored yet");
    let refused = fixture
        .gateway
        .set_credentials("keep", env(&[("PATH", "/tmp")]))
        .await;
    assert!(refused.is_err(), "only declared variables can be set");
    fixture
        .gateway
        .set_credentials("keep", env(&[("FAKE_KEY", "s3cr3t-value")]))
        .await
        .unwrap();
    assert!(fixture.refresh("keep").await.ok);
    let runtime = fixture.gateway.runtime("keep").unwrap();
    assert_eq!(runtime.status(), IntegrationStatus::Ready);
    let result = call(&fixture, "keep__env").await;
    let keys: Vec<String> =
        serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
    let allowed = [
        "FAKE_KEY",
        "HOME",
        "NODE_ENV",
        "PATH",
        "__CF_USER_TEXT_ENCODING",
        "LC_CTYPE",
    ];
    assert!(keys.contains(&"FAKE_KEY".to_owned()));
    assert!(
        keys.iter().all(|k| allowed.contains(&k.as_str())),
        "the child inherited {keys:?}"
    );
    let echoed = call(&fixture, "keep__echo").await;
    assert_eq!(echoed["content"][0]["text"], r#"{"a": 1}"#);
    fixture.stop().await;
}

#[tokio::test]
async fn a_crashed_server_is_restarted_and_its_diagnostics_are_redacted() {
    let (fixture, _dir) = stdio_fixture().await;
    fixture
        .gateway
        .set_credentials("keep", env(&[("FAKE_KEY", "s3cr3t-value")]))
        .await
        .unwrap();
    fixture.refresh("keep").await;
    let first = call(&fixture, "keep__echo").await["structuredContent"]["pid"].clone();
    let crashed = call(&fixture, "keep__crash").await;
    assert_eq!(crashed["isError"], json!(true));
    let message = crashed["content"][0]["text"].as_str().unwrap().to_owned();
    assert!(!message.contains("s3cr3t-value"), "{message}");
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let again = call(&fixture, "keep__echo").await;
    assert_ne!(again.get("isError"), Some(&json!(true)), "{again:?}");
    assert_ne!(again["structuredContent"]["pid"], first, "a new process");
    fixture.stop().await;
}
