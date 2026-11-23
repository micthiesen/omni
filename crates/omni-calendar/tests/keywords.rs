//! The calendar candidate keyword filter.
//!
//! User rules come from `omni_email` in production; here the recording
//! `FakeSupport` answers rule lookups for the calendar scope only, so
//! "parcel-scoped rules do not affect the calendar filter" is expressed as
//! "no calendar-scoped rule matched".
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use common::{FakeSupport, Triage, email};
use omni_calendar::filter::{FilterResult, filter_calendar_candidate};
use omni_calendar::support::{AdmitTier, RuleVerdict};

fn pass(reason: &str, admit_tier: AdmitTier) -> FilterResult {
    FilterResult::Pass {
        reason: reason.to_owned(),
        admit_tier,
    }
}

fn reject(reason: &str) -> FilterResult {
    FilterResult::Reject {
        reason: reason.to_owned(),
    }
}

async fn filter(
    support: &FakeSupport,
    from: &str,
    subject: &str,
    body: &str,
    self_address: Option<&str>,
) -> FilterResult {
    filter_calendar_candidate(
        &email("email-1", from, subject, body),
        support,
        self_address,
    )
    .await
    .unwrap()
}

// describe("filterCalendarCandidate — sender rules")

#[tokio::test]
async fn a_block_rule_beats_even_a_known_auto_pass_sender() {
    let support = FakeSupport::new(FakeSupport::calendar_yes());
    support.add_rule("eventbrite.com", RuleVerdict::Block);
    let result = filter(
        &support,
        "noreply@eventbrite.com",
        "Your event is coming up",
        "",
        None,
    )
    .await;
    assert_eq!(result, reject("blocked by rule eventbrite.com"));
}

#[tokio::test]
async fn an_allow_rule_passes_without_consulting_triage() {
    let support = FakeSupport::new(Triage::Down);
    support.add_rule("clinic.example", RuleVerdict::Allow);
    let result = filter(
        &support,
        "frontdesk@clinic.example",
        "Anything at all",
        "",
        None,
    )
    .await;
    assert_eq!(
        result,
        pass("allowed by rule clinic.example", AdmitTier::Rule)
    );
    assert_eq!(support.classify_calls(), 0);
}

#[tokio::test]
async fn parcel_scoped_rules_do_not_affect_the_calendar_filter() {
    let support = FakeSupport::new(Triage::Down);
    let result = filter(&support, "noreply@eventbrite.com", "anything", "", None).await;
    assert_eq!(result, pass("known sender", AdmitTier::Builtin));
}

#[tokio::test]
async fn an_allow_rule_overrides_the_built_in_blacklist() {
    let support = FakeSupport::new(Triage::Down);
    support.add_rule("steampowered.com", RuleVerdict::Allow);
    let result = filter(
        &support,
        "noreply@steampowered.com",
        "Purchase confirmation",
        "",
        None,
    )
    .await;
    assert_eq!(
        result,
        pass("allowed by rule steampowered.com", AdmitTier::Rule)
    );
}

// describe("filterCalendarCandidate — static blacklist")

#[tokio::test]
async fn rejects_newsletters_and_social_senders() {
    let support = FakeSupport::new(FakeSupport::calendar_yes());
    let result = filter(
        &support,
        "blockedandreported@substack.com",
        "Weekly Open Thread",
        "",
        None,
    )
    .await;
    assert_eq!(result, reject("blacklisted sender"));
}

#[tokio::test]
async fn rejects_food_delivery_senders_even_with_calendar_keywords_present() {
    let support = FakeSupport::new(FakeSupport::calendar_yes());
    let result = filter(
        &support,
        "no-reply@doordash.com",
        "Order Confirmation for Michael from DashMart",
        "",
        None,
    )
    .await;
    assert!(!result.passed());
}

#[tokio::test]
async fn rejects_steam_npm_and_ups_shipment_senders() {
    let support = FakeSupport::new(FakeSupport::calendar_yes());
    for from in [
        "noreply@steampowered.com",
        "support@npmjs.com",
        "pkginfo@ups.com",
    ] {
        let result = filter(
            &support,
            from,
            "Confirmation of your recent activity",
            "",
            None,
        )
        .await;
        assert_eq!(
            result,
            reject("blacklisted sender"),
            "Expected {from} to be rejected"
        );
    }
}

#[tokio::test]
async fn rejects_the_users_own_outgoing_address_when_configured() {
    let support = FakeSupport::new(FakeSupport::calendar_yes());
    let result = filter(
        &support,
        "michael@example.com",
        "Dinner reservation details",
        "",
        Some("Michael@Example.com"),
    )
    .await;
    assert_eq!(result, reject("blacklisted sender"));
}

// describe("filterCalendarCandidate — auto-pass senders")

#[tokio::test]
async fn passes_a_known_domain_without_consulting_triage() {
    let support = FakeSupport::new(Triage::Down);
    let result = filter(&support, "noreply@eventbrite.com", "anything", "", None).await;
    assert_eq!(result, pass("known sender", AdmitTier::Builtin));
    assert_eq!(support.classify_calls(), 0);
}

#[tokio::test]
async fn passes_a_transactional_subdomain_of_a_known_domain() {
    let support = FakeSupport::new(Triage::Down);
    let result = filter(
        &support,
        "noreply@reminder.eventbrite.com",
        "Just added! BCIMS New Year's Retreat",
        "",
        None,
    )
    .await;
    assert_eq!(result, pass("known sender", AdmitTier::Builtin));
}

#[tokio::test]
async fn passes_a_known_domain_wrapped_in_a_display_name_angle_bracket_form() {
    let support = FakeSupport::new(Triage::Down);
    let result = filter(
        &support,
        "\"Eventbrite Reminders\" <noreply@reminder.eventbrite.com>",
        "anything",
        "",
        None,
    )
    .await;
    assert_eq!(result, pass("known sender", AdmitTier::Builtin));
}

#[tokio::test]
async fn sends_a_lookalike_domain_to_triage_instead_of_auto_passing() {
    let support = FakeSupport::new(FakeSupport::calendar_no());
    let result = filter(
        &support,
        "noreply@noteventbrite.com",
        "Updates to Our Privacy Policy",
        "",
        None,
    )
    .await;
    assert_eq!(result, reject("triage: not an event"));
}

#[tokio::test]
async fn no_longer_auto_passes_google_com_corporate_mail() {
    let support = FakeSupport::new(FakeSupport::calendar_no());
    let result = filter(
        &support,
        "googleaistudio-noreply@google.com",
        "[Reminder] Secure your API access",
        "",
        None,
    )
    .await;
    assert_eq!(result, reject("triage: not an event"));
}

// describe("filterCalendarCandidate — triage")

#[tokio::test]
async fn passes_when_triage_says_calendar() {
    let support = FakeSupport::new(FakeSupport::calendar_yes());
    let result = filter(
        &support,
        "no-reply@cortico.health",
        "Your Dr. Hassan Salame Appointment",
        "",
        None,
    )
    .await;
    assert_eq!(
        result,
        pass("triage: upcoming appointment", AdmitTier::Triage)
    );
}

#[tokio::test]
async fn fails_when_triage_says_no_even_with_calendar_keywords_present() {
    let support = FakeSupport::new(FakeSupport::calendar_no());
    let result = filter(
        &support,
        "service@intl.paypal.com",
        "Reminder: your subscription renews",
        "",
        None,
    )
    .await;
    assert_eq!(result, reject("triage: not an event"));
}

// describe("filterCalendarCandidate — keyword fallback when triage is down")

#[tokio::test]
async fn passes_real_calendar_subjects_on_keywords() {
    let support = FakeSupport::new(Triage::Down);
    let result = filter(
        &support,
        "notifications@tribehome.com",
        "Reminder - Hard Surface Cleaning - Monday",
        "",
        None,
    )
    .await;
    assert_eq!(
        result,
        pass(
            "keyword \"reminder\" (triage unavailable)",
            AdmitTier::KeywordFallback
        )
    );
}

#[tokio::test]
async fn does_not_pass_on_an_all_rights_reserved_footer() {
    let support = FakeSupport::new(Triage::Down);
    let result = filter(
        &support,
        "service@intl.paypal.com",
        "Your way to pay with PayPal is set",
        "Thanks for setting up PayPal.\n© 2026 PayPal. All rights reserved.",
        None,
    )
    .await;
    assert_eq!(result, reject("no keyword match (triage unavailable)"));
}

#[tokio::test]
async fn does_not_pass_a_privacy_policy_email_that_merely_mentions_a_concert() {
    let support = FakeSupport::new(Triage::Down);
    let result = filter(
        &support,
        "no-reply@legal.spotify.com",
        "Updates to Our Privacy Policy",
        "We may process data when you attend a concert or live event.",
        None,
    )
    .await;
    assert!(!result.passed());
}
