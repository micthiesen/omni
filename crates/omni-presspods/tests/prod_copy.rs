//! Manual check against a copy of the production docstore (never the original):
//! `OMNI_PROD_COPY=/path/to/copy.db cargo test -p omni-presspods --test it prod_copy:: -- --ignored --nocapture`.
//!
//! Every `press-pods-episode` and `press-pods-job` row must decode into the
//! typed entities, recompute its primary key, and re-encode to the same
//! value (modulo explicit `undefined` fields in older rows, which read like
//! omitted ones). Byte identity is
//! reported for information. The feed and list payloads are also built from
//! the copy to prove the read paths accept every row.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::print_stdout)]

use omni_presspods::model::{PressPodsEpisode, PressPodsJob};
use omni_store::DocOps;
use omni_store::cbor::{self, JsValue};
use omni_store::entity::{Entity, EntityDescriptor};
use omni_testkit::TestStore;

fn strip_undefined(value: JsValue) -> JsValue {
    match value {
        JsValue::Object(map) => JsValue::Object(
            map.into_iter()
                .filter(|(_, v)| *v != JsValue::Undefined)
                .map(|(k, v)| (k, strip_undefined(v)))
                .collect(),
        ),
        JsValue::Array(items) => JsValue::Array(items.into_iter().map(strip_undefined).collect()),
        other => other,
    }
}

#[derive(Debug, Default)]
struct Report {
    rows: usize,
    decoded: usize,
    pk_ok: usize,
    value_equal: usize,
    byte_identical: usize,
    failures: Vec<String>,
}

async fn audit<E: Entity>(store: &omni_store::Store) -> Report {
    let rows = store
        .read(|docs| docs.get_raw_rows_by_prefix(&format!("${}#", E::NAME)))
        .await
        .unwrap();
    let descriptor = EntityDescriptor::of::<E>();
    let mut report = Report::default();
    for row in rows {
        report.rows += 1;
        let Some(data) = row.data.as_deref() else {
            report.failures.push(format!("{}: NULL data", row.pk));
            continue;
        };
        let original = cbor::decode(data).unwrap();
        let typed: E = match cbor::from_value(original.clone()) {
            Ok(typed) => typed,
            Err(e) => {
                report.failures.push(format!("{}: {e}", row.pk));
                continue;
            }
        };
        report.decoded += 1;
        if (descriptor.recompute_pk)(&original).as_deref() == Ok(row.pk.as_str()) {
            report.pk_ok += 1;
        } else {
            report
                .failures
                .push(format!("{}: primary key mismatch", row.pk));
        }
        let encoded = cbor::encode(&cbor::to_value(&typed).unwrap());
        if encoded == data {
            report.byte_identical += 1;
        }
        let reread = cbor::decode(&encoded).unwrap();
        if reread == strip_undefined(original) {
            report.value_equal += 1;
        } else {
            report
                .failures
                .push(format!("{}: re-encoded value differs", row.pk));
        }
    }
    report
}

#[tokio::test]
#[ignore = "needs OMNI_PROD_COPY pointing at a copy of the production docstore"]
async fn production_presspods_rows_decode_and_round_trip() {
    let source = std::env::var("OMNI_PROD_COPY").expect("OMNI_PROD_COPY");
    let copy = TestStore::from_fixture(std::path::Path::new(&source)).await;
    let episodes = audit::<PressPodsEpisode>(&copy.store).await;
    let jobs = audit::<PressPodsJob>(&copy.store).await;
    println!("press-pods-episode: {episodes:?}");
    println!("press-pods-job: {jobs:?}");
    for report in [&episodes, &jobs] {
        assert!(report.failures.is_empty(), "{:?}", report.failures);
        assert_eq!(report.decoded, report.rows);
        assert_eq!(report.pk_ok, report.rows);
        assert_eq!(report.value_equal, report.rows);
    }
    assert!(episodes.rows > 0, "the copy should contain episodes");

    // Typed collection reads accept every row (no skipped rows).
    let typed: Vec<PressPodsEpisode> = copy
        .store
        .read(|docs| omni_store::EntityOps::get_all::<PressPodsEpisode>(docs))
        .await
        .unwrap();
    let live = copy
        .store
        .read(|docs| docs.count_by_entity("press-pods-episode"))
        .await
        .unwrap();
    assert_eq!(typed.len() as u64, live);

    // URL identity drives dedup, replace-on-resubmit and checkpoint ids:
    // every stored canonical URL must be what the normalizer derives.
    let jobs_typed: Vec<PressPodsJob> = copy
        .store
        .read(|docs| omni_store::EntityOps::get_all::<PressPodsJob>(docs))
        .await
        .unwrap();
    let mut normalized_checked = 0;
    for (url, stored) in typed
        .iter()
        .map(|e| (&e.article_url, &e.normalized_url))
        .chain(jobs_typed.iter().map(|j| (&j.url, &j.normalized_url)))
    {
        if let Some(stored) = stored {
            assert_eq!(&omni_presspods::url::normalize_url(url), stored, "{url}");
            normalized_checked += 1;
        }
    }
    println!("normalizeUrl parity: {normalized_checked} stored canonical URLs match");
    let mut newest_first = typed;
    newest_first.sort_by_key(|e| std::cmp::Reverse(e.created_at));
    let feed = omni_presspods::rss::build_feed("https://pods.example.test", &newest_first, 0);
    assert_eq!(feed.matches("<item>").count(), newest_first.len().min(50));

    // With OMNI_PROD_RSS (a saved production feed of the same copy) the built
    // feed must match it byte for byte.
    if let Ok(saved_feed) = std::env::var("OMNI_PROD_RSS") {
        let expected = std::fs::read_to_string(saved_feed).unwrap();
        let start = feed.find("<lastBuildDate>").unwrap() + "<lastBuildDate>".len();
        let end = feed[start..].find("</lastBuildDate>").unwrap() + start;
        let normalized = format!("{}LAST_BUILD_DATE{}", &feed[..start], &feed[end..]);
        if normalized != expected {
            let at = normalized
                .bytes()
                .zip(expected.bytes())
                .position(|(a, b)| a != b)
                .unwrap_or(normalized.len().min(expected.len()));
            let from = at.saturating_sub(80);
            panic!(
                "feeds differ at byte {at}:\nrust: {:?}\nts:   {:?}",
                normalized.get(from..at + 80).unwrap_or(""),
                expected.get(from..at + 80).unwrap_or("")
            );
        }
        println!(
            "Rust feed matches the TS feed byte for byte ({} bytes)",
            expected.len()
        );
    }
}
