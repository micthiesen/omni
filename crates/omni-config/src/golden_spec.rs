//! Configuration fixtures: every case in the committed `tests/golden/config.json`
//! must decode to the recorded values, or fail as recorded.

use std::collections::BTreeMap;

use serde_json::Value;

use super::*;

/// Variables the fixture cases do not record.
const RUST_ONLY: &[&str] = &[
    "OMNI_DEBUG",
    "FFMPEG_PATH",
    "YT_DLP_PATH",
    "OMNI_GATEWAY_PUBLIC_URL",
];

fn env(case: &Value) -> BTreeMap<String, String> {
    case["env"]
        .as_object()
        .expect("env")
        .iter()
        .map(|(k, v)| (k.clone(), v.as_str().expect("string").to_owned()))
        .collect()
}

/// Each key's value as the summary renders it (derived fields applied).
fn rust_view(config: &Config) -> BTreeMap<&'static str, String> {
    let mut view: BTreeMap<&'static str, String> = config.entries().into_iter().collect();
    let derived = [
        ("EMAIL_SELF_ADDRESS", config.email_self_address()),
        (
            "PUSHOVER_LIVE_TOKEN",
            config.pushover_token(PushoverChannel::Live),
        ),
        (
            "PUSHOVER_CALENDAR_TOKEN",
            config.pushover_token(PushoverChannel::Calendar),
        ),
        (
            "PUSHOVER_RECS_TOKEN",
            config.pushover_token(PushoverChannel::Recs),
        ),
        (
            "PUSHOVER_PODCAST_TOKEN",
            config.pushover_token(PushoverChannel::Podcast),
        ),
        (
            "PUSHOVER_PRESSPODS_TOKEN",
            config.pushover_token(PushoverChannel::PressPods),
        ),
    ];
    for (key, value) in derived {
        view.insert(
            key,
            value.map_or_else(|| "undefined".to_owned(), str::to_owned),
        );
    }
    view
}

fn fixture_view(config: &Value) -> BTreeMap<String, String> {
    config
        .as_object()
        .expect("config object")
        .iter()
        .map(|(k, v)| {
            let rendered = match v {
                Value::String(s) => s.clone(),
                Value::Bool(b) => b.to_string(),
                Value::Number(n) => omni_core::js::number_to_string(n.as_f64().expect("f64")),
                Value::Object(_) => "***".to_owned(),
                other => panic!("unexpected config value {other}"),
            };
            (k.clone(), rendered)
        })
        .collect()
}

#[test]
fn every_golden_case_matches_its_fixture() {
    let cases: Vec<Value> =
        serde_json::from_str(include_str!("../tests/golden/config.json")).expect("golden");
    assert!(cases.len() > 70);
    let mut failures = Vec::new();
    for case in &cases {
        let name = case["name"].as_str().expect("name");
        let result = Config::from_env(&env(case));
        match (case["ok"].as_bool().expect("ok"), result) {
            (false, Ok(_)) => failures.push(format!("{name}: fixture rejects, parser accepts")),
            (true, Err(e)) => {
                failures.push(format!("{name}: fixture accepts, parser rejects: {e}"))
            }
            (false, Err(_)) => {}
            (true, Ok(config)) => {
                let expected = fixture_view(&case["config"]);
                let actual = rust_view(&config);
                for key in CONFIG_KEYS.iter().filter(|k| !RUST_ONLY.contains(k)) {
                    let want = expected.get(*key).map_or("undefined", String::as_str);
                    let got = actual.get(key).map_or("undefined", String::as_str);
                    if want != got {
                        failures.push(format!("{name}: {key} expected {want:?} got {got:?}"));
                    }
                }
                for key in expected.keys() {
                    if !CONFIG_KEYS.contains(&key.as_str()) {
                        failures.push(format!("{name}: expected key {key} missing"));
                    }
                }
                if let Some(whisker) = case["config"].get("WHISKER_CREDENTIALS") {
                    let creds = config.whisker_credentials.as_ref().expect("whisker");
                    assert_eq!(whisker["email"], creds.email.as_str());
                    assert_eq!(whisker["password"], creds.password.as_str());
                }
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

fn load(pairs: &[(&str, &str)]) -> Result<Config, ConfigError> {
    let vars = pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect();
    Config::from_env(&vars)
}

#[test]
fn rejects_sender_overrides_with_a_typed_error() {
    assert!(matches!(
        load(&[("EMAIL_FROM", "micthiesen@icloud.com")]),
        Err(ConfigError::EmailFrom(_))
    ));
    assert_eq!(
        load(&[("EMAIL_FROM", OUTGOING_EMAIL_FROM)])
            .expect("fixed sender")
            .email_from
            .as_deref(),
        Some(OUTGOING_EMAIL_FROM)
    );
}

#[test]
fn token_rules_have_typed_errors() {
    let strong = "0123456789abcdefghijklmnopqrstuv";
    assert!(matches!(
        load(&[("DOCKERIZED", "true")]),
        Err(ConfigError::Invalid {
            key: "OMNI_MCP_TOKEN",
            ..
        })
    ));
    assert!(matches!(
        load(&[("OMNI_MCP_TOKEN", "weak")]),
        Err(ConfigError::WeakToken("OMNI_MCP_TOKEN"))
    ));
    assert!(matches!(
        load(&[
            ("OMNI_MCP_TOKEN", strong),
            ("OMNI_DEVICE_LINK_TOKEN", strong)
        ]),
        Err(ConfigError::TokensEqual)
    ));
}

#[test]
fn redacts_secrets_and_identifying_account_fields() {
    let config = load(&[
        ("PUSHOVER_USER", "private-user-id"),
        ("PUSHOVER_TOKEN", "private-token"),
        ("ICLOUD_USERNAME", "private@example.com"),
        ("ICLOUD_APP_PASSWORD", "private-icloud-password"),
        (
            "WHISKER_CREDENTIALS",
            "private@example.com:private-password",
        ),
        ("SMTP_USER", "private-smtp"),
    ])
    .expect("config");
    let summary = format!("{:?} {config:?}", config.redacted_summary());
    for secret in [
        "private-user-id",
        "private-token",
        "private@example.com",
        "private-password",
        "private-icloud-password",
        "private-smtp",
    ] {
        assert!(!summary.contains(secret), "{secret} leaked");
    }
    assert!(summary.contains("***"));
    assert!(summary.contains("America/Vancouver"));
}

#[test]
fn binary_paths_default_like_js_or() {
    let config = load(&[("FFMPEG_PATH", ""), ("YT_DLP_PATH", "/opt/yt-dlp")]).expect("config");
    assert_eq!(config.ffmpeg_bin(), "ffmpeg");
    assert_eq!(config.yt_dlp_bin(), "/opt/yt-dlp");
    assert!(!config.omni_debug);
    assert!(load(&[("OMNI_DEBUG", "")]).expect("config").omni_debug);
}

#[test]
fn db_path_follows_sqlite_layer_config() {
    let local = load(&[]).expect("config");
    assert_eq!(local.db_path(), std::path::PathBuf::from("docstore.db"));
    let docker = load(&[
        ("DOCKERIZED", "true"),
        ("DB_NAME", "docstore.db"),
        ("OMNI_MCP_TOKEN", "0123456789abcdefghijklmnopqrstuv"),
    ])
    .expect("config");
    assert_eq!(
        docker.db_path(),
        std::path::PathBuf::from("/data/docstore.db")
    );
}
