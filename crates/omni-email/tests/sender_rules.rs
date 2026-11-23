//! Store-backed sender rules (pattern matching and normalization are unit
//! tests in `src/sender_rules.rs`).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use omni_email::sender_rules::{
    self, RuleError, RuleScope, RuleTarget, RuleVerdict, find_sender_rule, sender_rule_verdict,
};

use common::{NOW, store_at};

#[tokio::test(start_paused = true)]
async fn returns_undefined_when_no_rule_matches() {
    let (store, _clock) = store_at(NOW).await;
    sender_rules::upsert(
        &store.store,
        "other.com",
        RuleScope::Both,
        RuleVerdict::Block,
    )
    .await
    .unwrap();
    assert_eq!(
        sender_rule_verdict(&store.store, "orders@shop.com", RuleTarget::Parcel)
            .await
            .unwrap(),
        None
    );
}

#[tokio::test(start_paused = true)]
async fn only_applies_rules_whose_scope_covers_the_pipeline() {
    let (store, _clock) = store_at(NOW).await;
    sender_rules::upsert(
        &store.store,
        "shop.com",
        RuleScope::Parcel,
        RuleVerdict::Block,
    )
    .await
    .unwrap();
    assert_eq!(
        sender_rule_verdict(&store.store, "orders@shop.com", RuleTarget::Parcel)
            .await
            .unwrap(),
        Some(RuleVerdict::Block)
    );
    assert_eq!(
        sender_rule_verdict(&store.store, "orders@shop.com", RuleTarget::Calendar)
            .await
            .unwrap(),
        None
    );
}

#[tokio::test(start_paused = true)]
async fn applies_both_scoped_rules_to_either_pipeline() {
    let (store, _clock) = store_at(NOW).await;
    sender_rules::upsert(
        &store.store,
        "shop.com",
        RuleScope::Both,
        RuleVerdict::Allow,
    )
    .await
    .unwrap();
    for target in [RuleTarget::Parcel, RuleTarget::Calendar] {
        assert_eq!(
            sender_rule_verdict(&store.store, "orders@shop.com", target)
                .await
                .unwrap(),
            Some(RuleVerdict::Allow)
        );
    }
}

#[tokio::test(start_paused = true)]
async fn block_beats_allow_when_multiple_rules_match() {
    let (store, _clock) = store_at(NOW).await;
    sender_rules::upsert(
        &store.store,
        "shop.com",
        RuleScope::Parcel,
        RuleVerdict::Allow,
    )
    .await
    .unwrap();
    sender_rules::upsert(
        &store.store,
        "orders@shop.com",
        RuleScope::Parcel,
        RuleVerdict::Block,
    )
    .await
    .unwrap();
    assert_eq!(
        sender_rule_verdict(&store.store, "orders@shop.com", RuleTarget::Parcel)
            .await
            .unwrap(),
        Some(RuleVerdict::Block)
    );
    assert_eq!(
        find_sender_rule(&store.store, "orders@shop.com", RuleTarget::Parcel)
            .await
            .unwrap()
            .unwrap()
            .pattern,
        "orders@shop.com"
    );
}

#[tokio::test(start_paused = true)]
async fn normalizes_the_pattern_and_derives_the_rule_id() {
    let (store, _clock) = store_at(NOW).await;
    let row = sender_rules::upsert(
        &store.store,
        "  Orders@Shop.COM ",
        RuleScope::Parcel,
        RuleVerdict::Block,
    )
    .await
    .unwrap();
    assert_eq!(row.pattern, "orders@shop.com");
    assert_eq!(row.rule_id, "parcel:orders@shop.com");
}

#[tokio::test(start_paused = true)]
async fn rejects_an_empty_pattern() {
    let (store, _clock) = store_at(NOW).await;
    let result =
        sender_rules::upsert(&store.store, "  ", RuleScope::Both, RuleVerdict::Block).await;
    assert!(matches!(result, Err(RuleError::EmptyPattern)));
}

#[tokio::test(start_paused = true)]
async fn overwrites_the_verdict_on_re_upsert_and_keeps_a_single_row() {
    let (store, _clock) = store_at(NOW).await;
    sender_rules::upsert(
        &store.store,
        "shop.com",
        RuleScope::Both,
        RuleVerdict::Block,
    )
    .await
    .unwrap();
    let updated = sender_rules::upsert(
        &store.store,
        "shop.com",
        RuleScope::Both,
        RuleVerdict::Allow,
    )
    .await
    .unwrap();
    assert_eq!(updated.verdict, RuleVerdict::Allow);
    assert_eq!(sender_rules::list(&store.store).await.unwrap().len(), 1);
}

#[tokio::test(start_paused = true)]
async fn deletes_by_rule_id_and_reports_whether_the_rule_existed() {
    let (store, _clock) = store_at(NOW).await;
    let row = sender_rules::upsert(
        &store.store,
        "shop.com",
        RuleScope::Both,
        RuleVerdict::Block,
    )
    .await
    .unwrap();
    assert!(
        sender_rules::delete(&store.store, &row.rule_id)
            .await
            .unwrap()
    );
    assert!(
        !sender_rules::delete(&store.store, &row.rule_id)
            .await
            .unwrap()
    );
    assert!(sender_rules::list(&store.store).await.unwrap().is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_both_add_folds_in_and_removes_existing_single_scope_rules() {
    let (store, _clock) = store_at(NOW).await;
    sender_rules::upsert(
        &store.store,
        "@x.com",
        RuleScope::Parcel,
        RuleVerdict::Block,
    )
    .await
    .unwrap();
    let result =
        sender_rules::upsert_checked(&store.store, "@x.com", RuleScope::Both, RuleVerdict::Allow)
            .await
            .unwrap();
    assert!(result.merged);
    let rules = sender_rules::list(&store.store).await.unwrap();
    assert_eq!(rules.len(), 1);
    assert_eq!(rules[0].scope, RuleScope::Both);
    assert_eq!(rules[0].verdict, RuleVerdict::Allow);
    // The contradictory parcel block is gone, so the sender is truly allowed.
    assert_eq!(
        sender_rule_verdict(&store.store, "a@x.com", RuleTarget::Parcel)
            .await
            .unwrap(),
        Some(RuleVerdict::Allow)
    );
}

#[tokio::test(start_paused = true)]
async fn reports_an_exact_same_verdict_duplicate_as_already_existing() {
    let (store, _clock) = store_at(NOW).await;
    sender_rules::upsert_checked(
        &store.store,
        "@x.com",
        RuleScope::Parcel,
        RuleVerdict::Block,
    )
    .await
    .unwrap();
    let again = sender_rules::upsert_checked(
        &store.store,
        "@x.com",
        RuleScope::Parcel,
        RuleVerdict::Block,
    )
    .await
    .unwrap();
    assert!(again.already_exists);
    assert_eq!(sender_rules::list(&store.store).await.unwrap().len(), 1);
}

#[tokio::test(start_paused = true)]
async fn merges_opposite_single_scope_rules_into_both() {
    let (store, clock) = store_at(1_000).await;
    sender_rules::upsert_checked(
        &store.store,
        "@x.com",
        RuleScope::Calendar,
        RuleVerdict::Block,
    )
    .await
    .unwrap();
    clock.set(5_000);
    let merged = sender_rules::upsert_checked(
        &store.store,
        "@x.com",
        RuleScope::Parcel,
        RuleVerdict::Block,
    )
    .await
    .unwrap();
    assert!(merged.merged);
    assert_eq!(merged.rule.rule_id, "both:@x.com");
    assert_eq!(merged.rule.created_at, 1_000);
    assert_eq!(sender_rules::list(&store.store).await.unwrap().len(), 1);
}
