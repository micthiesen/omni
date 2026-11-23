//! Control registration persistence.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use omni_ios_controls::persistence::{
    delete_registration, list_registrations, mark_delivered, replace_device_registrations,
};

#[tokio::test]
async fn replaces_one_device_without_touching_another() {
    let (store, _) = common::store(0).await;
    let s = &store.store;
    replace_device_registrations(
        s,
        "device-one",
        vec![common::control("old", 1, &"a".repeat(64))],
    )
    .await
    .unwrap();
    replace_device_registrations(
        s,
        "device-two",
        vec![common::control("keep", 2, &"b".repeat(64))],
    )
    .await
    .unwrap();
    replace_device_registrations(
        s,
        "device-one",
        vec![common::control("new", 4, &"c".repeat(64))],
    )
    .await
    .unwrap();
    let mut ids: Vec<String> = list_registrations(s)
        .await
        .unwrap()
        .into_iter()
        .map(|r| r.registration_id)
        .collect();
    ids.sort();
    assert_eq!(ids, ["device-one:new", "device-two:keep"]);
}

#[tokio::test]
async fn preserves_delivered_state_only_while_token_configuration_is_unchanged() {
    let (store, _) = common::store(0).await;
    let s = &store.store;
    let a = "a".repeat(64);
    let b = "b".repeat(64);
    replace_device_registrations(s, "device-one", vec![common::control("slot-one", 1, &a)])
        .await
        .unwrap();
    mark_delivered(s, "device-one:slot-one", &a, "state-hash")
        .await
        .unwrap();
    let unchanged =
        replace_device_registrations(s, "device-one", vec![common::control("slot-one", 1, &a)])
            .await
            .unwrap();
    assert_eq!(
        unchanged[0].last_delivered_hash.as_deref(),
        Some("state-hash")
    );
    let rotated =
        replace_device_registrations(s, "device-one", vec![common::control("slot-one", 1, &b)])
            .await
            .unwrap();
    assert_eq!(rotated[0].last_delivered_hash, None);
    mark_delivered(s, "device-one:slot-one", &a, "stale")
        .await
        .unwrap();
    delete_registration(s, "device-one:slot-one", &a)
        .await
        .unwrap();
    let rows = list_registrations(s).await.unwrap();
    assert_eq!(rows[0].push_token, b);
    assert_eq!(rows[0].last_delivered_hash, None);
}

#[tokio::test]
async fn never_commits_a_mixed_registration_set_under_concurrent_replacements() {
    let (store, _) = common::store(0).await;
    let s = &store.store;
    let first = vec![
        common::control("a", 1, &"a".repeat(64)),
        common::control("b", 2, &"b".repeat(64)),
    ];
    let second = vec![
        common::control("c", 3, &"c".repeat(64)),
        common::control("d", 4, &"d".repeat(64)),
    ];
    let (one, two) = tokio::join!(
        replace_device_registrations(s, "device-one", first),
        replace_device_registrations(s, "device-one", second)
    );
    one.unwrap();
    two.unwrap();
    let mut ids: Vec<String> = list_registrations(s)
        .await
        .unwrap()
        .into_iter()
        .map(|r| r.control_id)
        .collect();
    ids.sort();
    assert!(ids == ["a", "b"] || ids == ["c", "d"], "{ids:?}");
}
