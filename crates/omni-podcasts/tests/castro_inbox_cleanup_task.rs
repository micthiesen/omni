//! The `CastroInboxCleanup` task.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use std::sync::Arc;

use common::{FakeAccount, FakeAccounts};
use jiff::tz::TimeZone;
use omni_podcasts::account::{InboxEpisode, Unavailable};
use omni_podcasts::castro::cleanup::{
    CastroInboxCleanupTask, FREE_PREVIEW_DESCRIPTION_PREFIX, SCHEDULE, is_free_preview_episode,
};
use omni_tasks::{CronSchedule, Task as _};

fn inbox_episode(description: Option<&str>) -> InboxEpisode {
    InboxEpisode {
        client_episode_id: "11111111-1111-4111-8111-111111111111".into(),
        show_title: "Example Show".into(),
        episode_title: "Example Episode".into(),
        episode_guid: Some("episode-guid".into()),
        description: description.map(str::to_owned),
    }
}

fn task(account: Arc<FakeAccount>) -> CastroInboxCleanupTask {
    CastroInboxCleanupTask::new(
        CronSchedule::parse(SCHEDULE, &TimeZone::UTC).unwrap(),
        Arc::new(FakeAccounts(account)),
    )
}

#[test]
fn matches_only_descriptions_that_start_with_the_substack_preview_text() {
    assert!(is_free_preview_episode(&inbox_episode(Some(&format!(
        "{FREE_PREVIEW_DESCRIPTION_PREFIX} of a paid post."
    )))));
    assert!(!is_free_preview_episode(&inbox_episode(Some(&format!(
        "Intro. {FREE_PREVIEW_DESCRIPTION_PREFIX}"
    )))));
    assert!(!is_free_preview_episode(&inbox_episode(Some(
        "this is a free preview"
    ))));
    assert!(!is_free_preview_episode(&inbox_episode(None)));
}

#[tokio::test]
async fn clears_matching_inbox_entries_without_touching_the_queue() {
    let account = Arc::new(FakeAccount {
        inbox: Ok(vec![
            inbox_episode(Some(&format!(
                "{FREE_PREVIEW_DESCRIPTION_PREFIX} from Substack."
            ))),
            InboxEpisode {
                client_episode_id: "22222222-2222-4222-8222-222222222222".into(),
                ..inbox_episode(Some("A normal episode."))
            },
        ]),
        ..FakeAccount::default()
    });
    let task = task(account.clone());
    task.run_once().await.unwrap();
    assert_eq!(
        account
            .calls()
            .into_iter()
            .filter(|c| c.starts_with("clear_inbox_episode"))
            .collect::<Vec<_>>(),
        vec!["clear_inbox_episode:11111111-1111-4111-8111-111111111111"]
    );
    assert_eq!(account.count("dequeue_episode"), 0);
    assert_eq!(account.count("fetch_queue"), 0);
    assert_eq!(
        task.last_run_summary().as_deref(),
        Some("cleared 1 free preview episode(s) from inbox")
    );
    assert_eq!(task.schedule().as_str(), "0 */6 * * *");
}

#[tokio::test]
async fn fails_rather_than_treating_an_unavailable_inbox_as_empty() {
    let account = Arc::new(FakeAccount {
        inbox: Err(Unavailable::new("Castro timed out")),
        ..FakeAccount::default()
    });
    let error = task(account.clone()).run_once().await.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("Castro inbox unavailable: Castro timed out"),
        "{error}"
    );
    assert_eq!(account.count("clear_inbox_episode"), 0);
}
