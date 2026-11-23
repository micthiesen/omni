//! Manual check against a copy of the production docstore (never the original):
//! `OMNI_PROD_COPY=/path/to/copy.db cargo test -p omni-personal --test it prod_copy:: -- --ignored --nocapture`.
//!
//! Every `codex-reset-delivery`, `claude-reset-delivery`, `printer-accepted-job`
//! and `pet-health-alert`
//! row must decode into its typed entity, recompute its primary key, and
//! re-encode to the same JS value; every `pets` / `pet_weight_history` row must
//! read through the typed tables.
#![allow(clippy::print_stdout, clippy::unwrap_used, clippy::expect_used)]

use omni_personal::pets::alerts::PetHealthAlert;
use omni_personal::pets::dismissals::PetHealthDismissal;
use omni_personal::pets::persistence::PetStore;
use omni_personal::printer::AcceptedPrintRecord;
use omni_personal::reset_alerts::{ClaudeResetDelivery, CodexResetDelivery};
use omni_store::entity::{self, Entity};
use omni_store::{DocOps, Store, StoreError};
use omni_testkit::TestStore;

struct Report {
    rows: usize,
    decoded: usize,
    pk_matches: usize,
    value_equal: usize,
    byte_identical: usize,
}

async fn check<E: Entity>(store: &Store) -> Report {
    store
        .read(|docs| {
            let raw = docs.get_docs_by_entity(E::NAME)?;
            let mut report = Report {
                rows: raw.len(),
                decoded: 0,
                pk_matches: 0,
                value_equal: 0,
                byte_identical: 0,
            };
            for (pk, value) in &raw {
                let Ok(typed) = omni_store::cbor::from_value::<E>(value.clone()) else {
                    println!("{}: {pk} failed to decode", E::NAME);
                    continue;
                };
                report.decoded += 1;
                if entity::pk::<E>(&typed.key()).is_ok_and(|computed| computed == *pk) {
                    report.pk_matches += 1;
                }
                let encoded = omni_store::cbor::to_value(&typed)
                    .ok()
                    .map(|v| omni_store::cbor::encode(&v));
                let reread = encoded
                    .as_deref()
                    .and_then(|bytes| omni_store::cbor::decode(bytes).ok());
                if reread.as_ref() == Some(value) {
                    report.value_equal += 1;
                } else {
                    println!("{}: {pk} re-encodes to a different value", E::NAME);
                }
                let stored = docs.get_raw_row(pk)?.and_then(|row| row.data);
                if stored.is_some() && stored == encoded {
                    report.byte_identical += 1;
                }
            }
            Ok::<_, StoreError>(report)
        })
        .await
        .unwrap()
}

#[tokio::test]
#[ignore = "needs OMNI_PROD_COPY pointing at a copy of the production docstore"]
async fn production_rows_decode_and_round_trip() {
    let source = std::env::var("OMNI_PROD_COPY").unwrap();
    let copy = TestStore::from_fixture(std::path::Path::new(&source)).await;
    for (name, report) in [
        (
            "codex-reset-delivery",
            check::<CodexResetDelivery>(&copy.store).await,
        ),
        (
            "claude-reset-delivery",
            check::<ClaudeResetDelivery>(&copy.store).await,
        ),
        (
            "printer-accepted-job",
            check::<AcceptedPrintRecord>(&copy.store).await,
        ),
        (
            "pet-health-alert",
            check::<PetHealthAlert>(&copy.store).await,
        ),
        (
            "pet-health-dismissal",
            check::<PetHealthDismissal>(&copy.store).await,
        ),
    ] {
        println!(
            "{name}: {} rows, {} decoded, {} pk matches, {} value-equal, {} byte-identical",
            report.rows,
            report.decoded,
            report.pk_matches,
            report.value_equal,
            report.byte_identical
        );
        assert_eq!(report.decoded, report.rows, "{name}");
        assert_eq!(report.pk_matches, report.rows, "{name}");
        assert_eq!(report.value_equal, report.rows, "{name}");
    }

    let pets = PetStore::open(&copy.store).await.unwrap();
    let all = pets.all_pets_with_history().await.unwrap();
    let readings: usize = all.iter().map(|p| p.weight_history.len()).sum();
    let raw_readings: i64 = copy
        .store
        .read(|docs| {
            docs.connection()
                .query_row("SELECT count(*) FROM pet_weight_history", [], |row| {
                    row.get(0)
                })
                .map_err(|e| StoreError::Sqlite(e.to_string()))
        })
        .await
        .unwrap();
    println!(
        "pets: {} pets, {readings} readings (table has {raw_readings})",
        all.len()
    );
    assert_eq!(readings as i64, raw_readings);
    for entry in &all {
        let visits = pets.daily_visit_counts(&entry.pet.pet_id).await.unwrap();
        let total: u32 = visits.iter().map(|v| v.count).sum();
        assert_eq!(total as usize, entry.weight_history.len());
    }
}

/// Replays the health rules hourly over the production history (from April,
/// once 90-day baselines exist) with in-memory alert state, prints every push
/// they would have sent, and checks the calibration: the current declines and
/// the late-September outage alert, while the July-August plateau stays quiet.
#[tokio::test]
#[ignore = "needs OMNI_PROD_COPY pointing at a copy of the production docstore"]
async fn health_rules_replay_production_history() {
    use std::collections::HashMap;

    use omni_api::pets::PetHealthKind;
    use omni_personal::pets::health::{
        DAY_MS, HOUR_MS, Notice, RuleState, assess_gap, assess_pet, decide, readings,
    };

    let source = std::env::var("OMNI_PROD_COPY").unwrap();
    let copy = TestStore::from_fixture(std::path::Path::new(&source)).await;
    let all = PetStore::open(&copy.store)
        .await
        .unwrap()
        .all_pets_with_history()
        .await
        .unwrap();
    let parsed: Vec<_> = all.iter().map(|p| readings(&p.weight_history)).collect();
    let mut household: Vec<i64> = parsed.iter().flatten().map(|r| r.at_ms).collect();
    household.sort_unstable();
    let ms = |text: &str| text.parse::<jiff::Timestamp>().unwrap().as_millisecond();
    let tz = jiff::tz::TimeZone::get("America/Vancouver").unwrap();

    let mut states: HashMap<(String, PetHealthKind), RuleState> = HashMap::new();
    let mut pushes: Vec<(i64, String, PetHealthKind, Notice)> = Vec::new();
    let end = *household.last().unwrap();
    let mut now = ms("2026-04-01T00:00:00Z");
    while now <= end + HOUR_MS {
        let known: Vec<i64> = household.iter().copied().filter(|&at| at <= now).collect();
        let mut assessments = Vec::new();
        for (pet, readings) in all.iter().zip(&parsed) {
            let known_readings: Vec<_> = readings
                .iter()
                .copied()
                .filter(|r| r.at_ms <= now)
                .collect();
            assessments.extend(assess_pet(
                &pet.pet.pet_id,
                &pet.pet.name,
                &known_readings,
                &known,
                now,
            ));
        }
        assessments.push(assess_gap(&known, now, &tz));
        for a in assessments {
            let key = (a.pet_id.clone(), a.kind);
            let (next, notice) = decide(a.kind, states.get(&key), &a.signal, now);
            if let Some(next) = next {
                states.insert(key, next);
            }
            if let Some(notice) = notice {
                let name = all
                    .iter()
                    .find(|p| p.pet.pet_id == a.pet_id)
                    .map_or("household".to_owned(), |p| p.pet.name.clone());
                pushes.push((now, name, a.kind, notice));
            }
        }
        now += HOUR_MS;
    }
    for (at, name, kind, notice) in &pushes {
        println!(
            "{} {name} {} {}: {}",
            omni_core::js::to_iso_string(*at),
            kind.as_str(),
            notice.title(),
            notice.message()
        );
    }
    let count = |name: &str, kind: PetHealthKind, from: &str, to: &str| {
        pushes
            .iter()
            .filter(|(at, n, k, _)| n == name && *k == kind && *at >= ms(from) && *at < ms(to))
            .count()
    };
    // Sam's early-October two-week decline, once.
    assert_eq!(
        count(
            "Sam",
            PetHealthKind::WeightDrop2w,
            "2026-10-01T00:00:00Z",
            "2026-10-10T00:00:00Z"
        ),
        1
    );
    // Sandy's long decline, once over September and October.
    assert_eq!(
        count(
            "Sandy",
            PetHealthKind::WeightDrop90d,
            "2026-09-01T00:00:00Z",
            "2026-10-10T00:00:00Z"
        ),
        1
    );
    // The 09-27..10-04 outage alerts and recovers once.
    assert_eq!(
        count(
            "household",
            PetHealthKind::DataGap,
            "2026-09-28T00:00:00Z",
            "2026-10-06T00:00:00Z"
        ),
        2
    );
    // No weight alert fires weekly through the July-August plateau.
    for kind in [PetHealthKind::WeightDrop2w, PetHealthKind::WeightDrop90d] {
        for name in ["Sam", "Sandy"] {
            assert!(
                count(name, kind, "2026-07-01T00:00:00Z", "2026-08-25T00:00:00Z") == 0,
                "{name} {}",
                kind.as_str()
            );
        }
    }
    // At most one push per pet and rule per week.
    for (i, (at, name, kind, notice)) in pushes.iter().enumerate() {
        if matches!(notice, Notice::Recovery { .. }) {
            continue;
        }
        assert!(
            !pushes[..i].iter().any(|(earlier, n, k, e)| n == name
                && k == kind
                && !matches!(e, Notice::Recovery { .. })
                && at - earlier < 7 * DAY_MS),
            "{name} {} twice within a week",
            kind.as_str()
        );
    }
}
