//! The long-poll job relay: claims, withdrawals, results and timeouts.
//!
//! Tests run on a paused tokio runtime and advance it with
//! `tokio::time::advance`; the service's wall clock is an
//! `omni_core::clock::TestClock` that follows paused time.

#![allow(clippy::unwrap_used)]

use std::time::Duration;

use omni_core::clock::TestClock;
use omni_device_link::{
    DeviceCommand, DeviceJob, DeviceJobOutcome, DeviceLinkError, DeviceLinkService, PollReport,
};
use serde_json::{Map, Value, json};
use tokio::task::JoinHandle;

fn service() -> DeviceLinkService {
    DeviceLinkService::new(TestClock::new(1_790_000_000_000))
}

fn online() -> PollReport {
    PollReport {
        disabled: false,
        host: Some("MaxBook".to_owned()),
    }
}

fn args(value: Value) -> Map<String, Value> {
    value.as_object().cloned().unwrap_or_default()
}

async fn settle() {
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
}

fn spawn_poll(link: &DeviceLinkService, report: PollReport) -> JoinHandle<Vec<DeviceJob>> {
    let link = link.clone();
    tokio::spawn(async move { link.poll(report).await })
}

fn spawn_execute(
    link: &DeviceLinkService,
    command: DeviceCommand,
    arguments: Value,
    timeout: Duration,
) -> JoinHandle<Result<Map<String, Value>, DeviceLinkError>> {
    let link = link.clone();
    tokio::spawn(async move { link.execute(command, args(arguments), timeout).await })
}

/// Marks the host as seen with a poll held to its deadline.
async fn check_in(link: &DeviceLinkService, report: PollReport) -> Vec<DeviceJob> {
    let poll = spawn_poll(link, report);
    settle().await;
    tokio::time::advance(Duration::from_secs(25)).await;
    poll.await.unwrap()
}

#[tokio::test(start_paused = true)]
async fn refuses_commands_while_the_mac_has_never_checked_in() {
    let link = service();
    let error = link
        .execute(DeviceCommand::List, Map::new(), Duration::from_secs(5))
        .await
        .unwrap_err();
    assert_eq!(error.code, "offline");
    assert!(error.detail.contains("last seen never"));
    assert!(!link.status().online);
}

#[tokio::test(start_paused = true)]
async fn delivers_a_job_to_a_waiting_poll_and_returns_the_envelope_data() {
    let link = service();
    let poll = spawn_poll(&link, online());
    settle().await;
    let call = spawn_execute(
        &link,
        DeviceCommand::Status,
        json!({"session": "abc123"}),
        Duration::from_secs(5),
    );
    let jobs = poll.await.unwrap();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].command, DeviceCommand::Status);
    assert_eq!(jobs[0].args, args(json!({"session": "abc123"})));
    let accepted = link.complete(
        &jobs[0].id,
        DeviceJobOutcome::Output(
            json!({"v": 1, "ok": true, "data": {"session_id": "abc123-full"}}),
        ),
    );
    assert!(accepted);
    assert_eq!(
        call.await.unwrap().unwrap(),
        args(json!({"session_id": "abc123-full"}))
    );
    assert_eq!(link.status().pending_jobs, 0);
    assert!(!link.complete(
        &jobs[0].id,
        DeviceJobOutcome::Error {
            code: "x".into(),
            message: "y".into()
        }
    ));
}

#[tokio::test(start_paused = true)]
async fn maps_client_error_envelopes_and_local_agent_errors_to_typed_failures() {
    let link = service();
    let cases = [
        (
            DeviceJobOutcome::Output(json!({
                "v": 1, "ok": false,
                "error": {"code": "busy", "message": "mid-turn", "retryable": true}
            })),
            "busy",
        ),
        (
            DeviceJobOutcome::Error {
                code: "timeout".into(),
                message: "took too long".into(),
            },
            "timeout",
        ),
        (
            DeviceJobOutcome::Output(json!({"nope": true})),
            "bad_output",
        ),
    ];
    for (outcome, code) in cases {
        let poll = spawn_poll(&link, online());
        settle().await;
        let call = spawn_execute(
            &link,
            DeviceCommand::Send,
            json!({"session": "abc123"}),
            Duration::from_secs(5),
        );
        let jobs = poll.await.unwrap();
        assert!(link.complete(&jobs[0].id, outcome));
        let error = call.await.unwrap().unwrap_err();
        assert_eq!(error.code, code);
    }
}

#[tokio::test(start_paused = true)]
async fn withdraws_a_job_the_mac_never_picks_up_so_it_cannot_run_later() {
    let link = service();
    check_in(&link, online()).await;
    let call = spawn_execute(
        &link,
        DeviceCommand::Stop,
        json!({"session": "abc123"}),
        Duration::from_secs(5),
    );
    settle().await;
    tokio::time::advance(Duration::from_secs(31)).await;
    assert_eq!(call.await.unwrap().unwrap_err().code, "not_picked_up");
    let poll = spawn_poll(&link, online());
    settle().await;
    tokio::time::advance(Duration::from_secs(25)).await;
    assert_eq!(poll.await.unwrap(), Vec::<DeviceJob>::new());
}

#[tokio::test(start_paused = true)]
async fn reports_an_unknown_outcome_when_a_delivered_job_never_answers() {
    let link = service();
    let poll = spawn_poll(&link, online());
    settle().await;
    let call = spawn_execute(
        &link,
        DeviceCommand::Send,
        json!({"session": "abc123"}),
        Duration::from_secs(10),
    );
    let jobs = poll.await.unwrap();
    tokio::time::advance(Duration::from_secs(26)).await;
    assert_eq!(call.await.unwrap().unwrap_err().code, "outcome_unknown");
    assert!(!link.complete(&jobs[0].id, DeviceJobOutcome::Output(json!({}))));
}

#[tokio::test(start_paused = true)]
async fn honors_the_macs_kill_switch_and_the_online_window() {
    let link = service();
    let disabled = PollReport {
        disabled: true,
        host: Some("MaxBook".to_owned()),
    };
    assert_eq!(check_in(&link, disabled).await, Vec::<DeviceJob>::new());
    let error = link
        .execute(DeviceCommand::List, Map::new(), Duration::from_secs(5))
        .await
        .unwrap_err();
    assert_eq!(error.code, "disabled");

    check_in(&link, online()).await;
    assert!(link.status().online);
    tokio::time::advance(Duration::from_secs(46)).await;
    let status = link.status();
    assert!(!status.online);
    assert!(!status.disabled);
    assert_eq!(status.host.as_deref(), Some("MaxBook"));
    let error = link
        .execute(DeviceCommand::List, Map::new(), Duration::from_secs(5))
        .await
        .unwrap_err();
    assert_eq!(error.code, "offline");
}

#[tokio::test(start_paused = true)]
async fn releases_an_older_held_poll_when_a_newer_one_arrives() {
    let link = service();
    let stale = spawn_poll(&link, online());
    settle().await;
    tokio::time::advance(Duration::from_secs(1)).await;
    let fresh = spawn_poll(&link, online());
    settle().await;
    assert_eq!(stale.await.unwrap(), Vec::<DeviceJob>::new());
    let call = spawn_execute(
        &link,
        DeviceCommand::Projects,
        json!({}),
        Duration::from_secs(5),
    );
    let jobs = fresh.await.unwrap();
    assert_eq!(jobs[0].command, DeviceCommand::Projects);
    link.complete(
        &jobs[0].id,
        DeviceJobOutcome::Output(json!({"v": 1, "ok": true, "data": {"projects": []}})),
    );
    assert_eq!(call.await.unwrap().unwrap(), args(json!({"projects": []})));
}

#[tokio::test(start_paused = true)]
async fn keeps_waiting_for_a_result_past_the_command_timeout_by_a_slack_margin() {
    let link = service();
    let poll = spawn_poll(&link, online());
    settle().await;
    let call = spawn_execute(
        &link,
        DeviceCommand::Start,
        json!({"project": "omni"}),
        Duration::from_secs(10),
    );
    let jobs = poll.await.unwrap();
    tokio::time::advance(Duration::from_secs(20)).await;
    link.complete(
        &jobs[0].id,
        DeviceJobOutcome::Error {
            code: "timeout".into(),
            message: "claude-for-dot did not finish".into(),
        },
    );
    assert_eq!(call.await.unwrap().unwrap_err().code, "timeout");
}

#[tokio::test(start_paused = true)]
async fn a_dropped_execute_withdraws_its_queued_job() {
    let link = service();
    check_in(&link, online()).await;
    let call = spawn_execute(
        &link,
        DeviceCommand::List,
        json!({}),
        Duration::from_secs(5),
    );
    settle().await;
    assert_eq!(link.status().pending_jobs, 1);
    call.abort();
    let _ = call.await;
    assert_eq!(link.status().pending_jobs, 0);
    let poll = spawn_poll(&link, online());
    settle().await;
    tokio::time::advance(Duration::from_secs(25)).await;
    assert!(poll.await.unwrap().is_empty());
}
