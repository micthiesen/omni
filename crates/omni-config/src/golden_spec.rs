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
    "PROWLARR_URL",
    "PROWLARR_API_KEY",
    "PLEX_AUDIOBOOKS_LIBRARY",
    "AUDIOBOOK_INDEXER",
    "AUDIOBOOK_LANGUAGE",
    "DELUGE_HOST",
    "DELUGE_PORT",
    "DELUGE_USERNAME",
    "DELUGE_PASSWORD",
    "DELUGE_CERT_SHA256",
    "DELUGE_LABEL",
    "YOUTUBE_CLIENT_ID",
    "YOUTUBE_CLIENT_SECRET",
    "YOUTUBE_REDIRECT_URI",
    "CLAUDE_NOTIFY_TARGETS",
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
    let mut view: BTreeMap<&'static str, String> = config
        .summary_entries()
        .into_iter()
        .map(|entry| (entry.key, entry.value))
        .collect();
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
    assert!(load(&[("DOCKERIZED", "true")]).is_ok());
    assert!(matches!(
        load(&[("OMNI_DEVICE_LINK_TOKEN", "weak")]),
        Err(ConfigError::WeakToken("OMNI_DEVICE_LINK_TOKEN"))
    ));
    assert!(load(&[("OMNI_DEVICE_LINK_TOKEN", "0123456789abcdefghijklmnopqrstuv")]).is_ok());
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
    let docker = load(&[("DOCKERIZED", "true"), ("DB_NAME", "docstore.db")]).expect("config");
    assert_eq!(
        docker.db_path(),
        std::path::PathBuf::from("/data/docstore.db")
    );
}

fn summary_map(config: &Config) -> BTreeMap<&'static str, String> {
    config.redacted_summary().into_iter().collect()
}

/// Every variable set to a marker that names it, minus those whose validation
/// rejects free text, so a key added later is covered without editing this test.
fn marked_config() -> (Config, BTreeMap<String, String>) {
    let mut vars: BTreeMap<String, String> = CONFIG_KEYS
        .iter()
        .map(|key| ((*key).to_owned(), format!("marker-{key}-value")))
        .collect();
    loop {
        match Config::from_env(&vars) {
            Ok(config) => return (config, vars),
            Err(ConfigError::Invalid { key, .. } | ConfigError::WeakToken(key)) => {
                vars.remove(key);
            }
            Err(ConfigError::EmailFrom(_)) => {
                vars.remove("EMAIL_FROM");
            }
        }
    }
}

#[test]
fn summary_hides_every_free_text_value_not_listed_public() {
    let (config, vars) = marked_config();
    let summary = summary_map(&config);
    let mut leaked = Vec::new();
    let mut shown = 0;
    for (key, marker) in &vars {
        let value = &summary[key.as_str()];
        if value.contains(marker.as_str()) {
            if PUBLIC_TEXT_KEYS.contains(&key.as_str()) {
                shown += 1;
            } else {
                leaked.push(key.clone());
            }
        }
    }
    assert!(leaked.is_empty(), "values logged for {leaked:?}");
    assert!(shown > 20, "public keys should still show values");
    // The keys from the original incident are covered by name.
    for key in [
        "PODCASTINDEX_KEY",
        "KICK_CLIENT_ID",
        "YOUTUBE_CLIENT_ID",
        "DELUGE_USERNAME",
    ] {
        assert!(vars.contains_key(key), "{key} not exercised");
        assert_eq!(summary[key], "***");
    }
}

#[test]
fn summary_hides_identifiers_with_validated_formats() {
    let castro = "6f1c2a9e-3b4d-4e5f-8a7b-9c0d1e2f3a4b";
    let sha = "ab".repeat(32);
    let config = load(&[
        ("CASTRO_ACCESS_ID", castro),
        ("DELUGE_CERT_SHA256", &sha),
        (
            "ICLOUD_CALENDAR_URL",
            "https://p01-caldav.icloud.com/123456789/calendars/work/",
        ),
    ])
    .expect("config");
    let summary = format!("{:?} {config:?}", config.redacted_summary());
    for secret in [castro, sha.as_str(), "123456789"] {
        assert!(!summary.contains(secret), "{secret} leaked");
    }
}

#[test]
fn summary_reports_presence_without_values() {
    let set = summary_map(&load(&[("EXA_API_KEY", "exa-live-key")]).expect("config"));
    let empty = summary_map(&load(&[("EXA_API_KEY", "")]).expect("config"));
    let unset = summary_map(&load(&[]).expect("config"));
    assert_eq!(set["EXA_API_KEY"], "***");
    assert_eq!(empty["EXA_API_KEY"], "");
    assert_eq!(unset["EXA_API_KEY"], "undefined");
}

#[test]
fn public_urls_drop_userinfo_query_and_fragment() {
    let config = load(&[
        (
            "PLEX_URL",
            "https://user:hunter2@plex.example:32400/web?X-Plex-Token=tok123#f",
        ),
        ("HISTER_URL", "https://hister.example/"),
    ])
    .expect("config");
    let summary = summary_map(&config);
    assert_eq!(
        summary["PLEX_URL"],
        "https://plex.example:32400/web (credentials, query or fragment hidden)"
    );
    assert_eq!(summary["HISTER_URL"], "https://hister.example");
    let all = format!("{summary:?}");
    for secret in ["hunter2", "tok123", "user:"] {
        assert!(!all.contains(secret), "{secret} leaked");
    }
}

#[test]
fn public_text_keys_are_real_and_never_secret_names() {
    for key in PUBLIC_TEXT_KEYS {
        assert!(CONFIG_KEYS.contains(key), "{key} is not a config key");
        assert!(
            !is_sensitive_key(key) && !PRIVATE_CONFIG_KEYS.contains(key),
            "{key} names a secret"
        );
    }
}

#[test]
fn secret_values_cover_hidden_text_and_skip_public_values() {
    let (config, vars) = marked_config();
    let secrets = config.secret_values();
    let marker = |key: &str| vars[key].clone();
    for key in [
        "PODCASTINDEX_KEY",
        "KICK_CLIENT_ID",
        "EXA_API_KEY",
        "PLEX_TOKEN",
    ] {
        assert!(secrets.contains(&marker(key)), "{key} not scrubbed");
    }
    for key in ["TZ", "PLEX_URL", "HISTER_URL"] {
        if let Some(value) = vars.get(key) {
            assert!(!secrets.contains(value), "{key} is public");
        }
    }
    let unset = load(&[]).expect("config").secret_values();
    assert!(!unset.iter().any(|v| v == "undefined"));
}

#[test]
fn secret_values_include_url_credentials_and_encoded_forms_but_not_identifiers() {
    let config = load(&[
        (
            "PRINTER_IPP_URL",
            "ipp://printer-user:printer-pass-1@10.0.0.2:631/ipp/print",
        ),
        (
            "NZBGET_URL",
            "http://nzbget:6789/jsonrpc?token=nzb-query-token-2",
        ),
        ("ANTHROPIC_API_KEY", "sk-ant-key+with/slash=3"),
        ("EMAIL_SELF_ADDRESS", "owner-address@example.com"),
    ])
    .expect("config");
    let secrets = config.secret_values();
    for expected in [
        "printer-user",
        "printer-pass-1",
        "nzb-query-token-2",
        "sk-ant-key+with/slash=3",
    ] {
        assert!(secrets.iter().any(|s| s == expected), "{expected} missing");
    }
    assert!(!secrets.iter().any(|s| s.contains("owner-address")));
    assert!(!secrets.iter().any(|s| s.contains("10.0.0.2")));
}
