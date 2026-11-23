//! Arr recovery boundary checks on paths and identities.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use omni_arr::arr_recovery::persistence::ActionPhase;
use omni_arr::arr_recovery::service::{
    notification_batches, notification_message, source_paths_confined,
};
use omni_arr::arr_recovery::{ArrKind, Decision, DecisionSource, Evidence, QueueItem, Target};

fn import(source: DecisionSource) -> Decision {
    Decision::Import {
        reason: String::new(),
        source,
    }
}

#[test]
fn never_drops_an_action_from_oversized_pushover_batches() {
    let actions: Vec<_> = (0..40)
        .map(|i| {
            let target = Target {
                title: format!("Series {i} {}", "x".repeat(500)),
                ..common::movie_target()
            };
            let mut action = common::action(
                &format!("d{i}"),
                target,
                import(DecisionSource::Llm),
                ActionPhase::Done,
                0,
            );
            action.title = format!("Release {i}");
            action
        })
        .collect();
    let batches = notification_batches(&actions);
    let flat: Vec<_> = batches.iter().flatten().cloned().collect();
    assert_eq!(flat, actions);
    assert!(
        batches
            .iter()
            .all(|batch| omni_core::js::utf16_len(&notification_message(batch)) <= 1000)
    );
}

#[test]
fn groups_many_imports_for_one_series_into_a_single_notification() {
    let target = Target {
        title: "House of Cards".into(),
        ..common::movie_target()
    };
    let actions: Vec<_> = (0..46)
        .map(|i| {
            common::action(
                &format!("d{i}"),
                target.clone(),
                import(DecisionSource::Rules),
                ActionPhase::Done,
                0,
            )
        })
        .collect();
    assert_eq!(notification_batches(&actions).len(), 1);
    assert!(notification_message(&actions).contains("46 downloads"));
}

#[test]
fn rejects_source_paths_in_containing_or_equal_to_the_library() {
    for output_path in [
        "/media/storage/sonarr/Show",
        "/media/storage/sonarr",
        "/media/storage/sonarr/Show/subdir",
    ] {
        let evidence = Evidence {
            download_health: None,
            kind: ArrKind::Sonarr,
            items: vec![QueueItem {
                output_path: Some(output_path.into()),
                ..QueueItem::target_ref(None, None, None)
            }],
            target: Target {
                path: "/media/storage/sonarr/Show".into(),
                ..common::movie_target()
            },
            files: vec![],
            grabs: vec![],
        };
        assert!(!source_paths_confined(&evidence), "{output_path}");
    }
}
