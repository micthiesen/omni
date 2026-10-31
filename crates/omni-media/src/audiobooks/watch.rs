//! `AudiobookWatch`: follows every open grab into the Plex library.
//!
//! Each run reads Deluge for the open grabs and decides each one's next
//! stage ([`decide`], pure). A download counts as finished only when Deluge
//! has moved it into the label's folder; then it must contain audio, Plex is
//! asked to scan exactly that folder, and the grab is ready once a Plex track
//! lives under it. Ready sends one notification, reserved as `sending` before
//! the push and never resent when uncertain; each problem notifies at most
//! once per grab.

use futures::future::BoxFuture;
use omni_alerts::PushoverMessage;
use omni_config::PushoverChannel;
use omni_store::StoreError;
use omni_store::entity::EntityWrite as _;
use omni_tasks::{CronSchedule, RunContext, Task, TaskError, TaskOptions};

use super::deluge::TorrentStatus;
use super::rank::words;
use super::{
    AudiobookGrabData, Audiobooks, HOUR_MS, LOG, Notice, Problem, Stage, has_audio, landed_path,
    ttl,
};
use crate::plex::PlexTrack;

pub const TASK_NAME: &str = "AudiobookWatch";
/// No progress for this long while downloading is a stall.
pub const STALL_AFTER_MS: i64 = 12 * HOUR_MS;
/// Finished this long without a Plex album is a problem.
pub const PLEX_GRACE_MS: i64 = 2 * HOUR_MS;
/// A finished download not yet in Plex is scanned again this often.
pub const RESCAN_EVERY_MS: i64 = 30 * 60 * 1000;
/// An uncertain add still absent from Deluge after this long never happened.
pub const NEVER_ADDED_AFTER_MS: i64 = 24 * HOUR_MS;
/// A torrent must stay absent from Deluge this long before it counts as removed.
pub const MISSING_AFTER_MS: i64 = 10 * 60 * 1000;
/// A finished download Plex still lacks after this long stops being followed.
pub const GIVE_UP_ON_PLEX_MS: i64 = 24 * HOUR_MS;
/// A reservation older than this without a settled add was interrupted.
const RESERVED_STALE_MS: i64 = 10 * 60 * 1000;

/// What Deluge said about a torrent this run.
#[derive(Clone, Copy, Debug)]
pub enum Observed<'a> {
    /// Deluge could not be read.
    Unknown,
    Absent,
    Present(&'a TorrentStatus),
}

/// One grab's next state and what to do about it.
#[derive(Clone, Debug, PartialEq)]
pub struct Decision {
    pub row: AudiobookGrabData,
    /// A folder to ask Plex to scan.
    pub scan: Option<String>,
    /// A problem to notify (not yet in `row.alerted`).
    pub alert: Option<Problem>,
}

fn problem(row: &mut AudiobookGrabData, problem: Problem, detail: String) -> Option<Problem> {
    row.problem = Some(problem);
    row.problem_detail = Some(detail);
    (!row.alerted.contains(&problem)).then_some(problem)
}

fn moved(status: &TorrentStatus) -> bool {
    status.is_finished
        && status.state != "Moving"
        && !status.move_completed_path.is_empty()
        && status.save_path.trim_end_matches('/')
            == status.move_completed_path.trim_end_matches('/')
}

/// The pure part of a run for one grab.
pub fn decide(current: &AudiobookGrabData, observed: Observed<'_>, now: i64) -> Decision {
    let mut row = current.clone();
    let mut scan = None;
    let mut alert = None;
    let stage = match row.stage {
        Stage::Reserved if now - row.updated_at > RESERVED_STALE_MS => Stage::Uncertain,
        stage => stage,
    };
    match (stage, observed) {
        (_, Observed::Unknown) | (Stage::Reserved, _) => {}
        (Stage::Uncertain, Observed::Absent) => {
            row.stage = Stage::Uncertain;
            if now - row.requested_at > NEVER_ADDED_AFTER_MS {
                row.stage = Stage::Failed;
                alert = problem(
                    &mut row,
                    Problem::NeverAdded,
                    "the torrent never appeared in Deluge after the grab".to_owned(),
                );
            }
        }
        (Stage::Uncertain | Stage::Downloading, Observed::Present(status)) => {
            row.stage = Stage::Downloading;
            row.missing_since = None;
            if row.problem == Some(Problem::DelugeError) && status.state != "Error" {
                row.problem = None;
                row.problem_detail = None;
            }
            if row.progress_at.is_none()
                || status.progress_percent > row.progress_percent.unwrap_or(-1.0) + 0.01
            {
                row.progress_percent = Some(status.progress_percent);
                row.progress_at = Some(now);
                if row.problem == Some(Problem::Stalled) {
                    row.problem = None;
                    row.problem_detail = None;
                }
            }
            if moved(status) {
                if has_audio(status) {
                    row.stage = Stage::Finished;
                    row.finished_at = Some(now);
                    row.progress_percent = Some(100.0);
                    row.problem = None;
                    row.problem_detail = None;
                    row.audio_files = audio_files(status);
                    let path = landed_path(status);
                    row.path = Some(path.clone());
                    scan = Some(path);
                } else {
                    row.stage = Stage::Failed;
                    alert = problem(
                        &mut row,
                        Problem::NoAudio,
                        format!(
                            "finished without any audio file (files are in {})",
                            landed_path(status)
                        ),
                    );
                }
            } else if status.state == "Error" {
                // Deluge's message can quote the tracker URL with its passkey.
                alert = problem(
                    &mut row,
                    Problem::DelugeError,
                    "Deluge reports an error for the torrent (details in Deluge)".to_owned(),
                );
            } else if now - row.progress_at.unwrap_or(now) > STALL_AFTER_MS {
                #[allow(clippy::cast_possible_truncation)]
                let percent = status.progress_percent.round() as i64;
                alert = problem(
                    &mut row,
                    Problem::Stalled,
                    format!(
                        "stuck at {percent}% for over 12 hours ({} seeders connected)",
                        status.seeds
                    ),
                );
            }
        }
        (Stage::Downloading, Observed::Absent) => match row.missing_since {
            // Absent on runs at least MISSING_AFTER_MS apart, not one empty read.
            Some(since) if now - since >= MISSING_AFTER_MS => {
                row.stage = Stage::Failed;
                alert = problem(
                    &mut row,
                    Problem::Missing,
                    "removed from Deluge before it finished".to_owned(),
                );
            }
            Some(_) => {}
            None => row.missing_since = Some(now),
        },
        (
            Stage::Finished | Stage::Ready | Stage::Cancelled | Stage::Failed,
            Observed::Absent | Observed::Present(_),
        ) => {}
    }
    if row != *current {
        row.updated_at = now;
    }
    Decision { row, scan, alert }
}

fn components(path: &str) -> Vec<Vec<String>> {
    path.split('/')
        .map(words)
        .filter(|w| !w.is_empty())
        .collect()
}

/// The Plex album holding every one of `files`, or `None` while any is
/// missing. Paths are compared by words, since Plex can report characters
/// differently from the torrent.
pub fn album_with(
    tracks: &[PlexTrack],
    files: &[String],
) -> Option<(Option<String>, Option<String>)> {
    if files.is_empty() {
        return None;
    }
    let mut album = None;
    for file in files {
        let wanted = components(file);
        if wanted.len() < 2 {
            return None;
        }
        let track = tracks
            .iter()
            .find(|t| t.files.iter().any(|f| components(f) == wanted))?;
        album.get_or_insert((track.album.clone(), track.artist.clone()));
    }
    album
}

fn audio_files(status: &TorrentStatus) -> Vec<String> {
    status
        .file_paths()
        .into_iter()
        .filter(|f| super::is_audio(f))
        .collect()
}

/// What one run did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WatchReport {
    pub open: usize,
    pub ready: usize,
    pub alerts: usize,
    /// An upstream could not be read, so some grabs were not advanced.
    pub degraded: Option<String>,
}

impl Audiobooks {
    /// One watch run at `now`.
    pub async fn watch(&self, now: i64) -> Result<WatchReport, StoreError> {
        let _writes = self.writes.lock().await;
        let mut report = WatchReport::default();
        let rows: Vec<AudiobookGrabData> = self
            .grabs()
            .await?
            .into_iter()
            .filter(|g| g.stage.is_open() || (g.stage == Stage::Ready && g.ready_notice.is_none()))
            .collect();
        report.open = rows.iter().filter(|g| g.stage.is_open()).count();
        if rows.is_empty() {
            return Ok(report);
        }
        let hashes: Vec<String> = rows
            .iter()
            .filter(|g| g.stage.is_open())
            .map(|g| g.info_hash.clone())
            .collect();
        let statuses = match self.deluge.statuses(&hashes).await {
            Ok(statuses) => Some(statuses),
            Err(error) => {
                tracing::warn!(target: LOG, "Audiobook watch could not read Deluge: {error}");
                report.degraded = Some(error.to_string());
                None
            }
        };
        let section = self.section().await;
        let mut tracks: Option<Vec<PlexTrack>> = None;
        for current in rows {
            let observed = match &statuses {
                None => Observed::Unknown,
                Some(s) => s
                    .get(&current.info_hash)
                    .map_or(Observed::Absent, Observed::Present),
            };
            let Decision {
                mut row,
                scan,
                alert,
            } = decide(&current, observed, now);
            if let (Some(path), Ok(section)) = (&scan, &section) {
                self.scan(&section.key, path).await;
                row.scanned_at = Some(now);
            }
            if row.stage == Stage::Finished {
                match &section {
                    Err(reason) => report.degraded = Some(reason.clone()),
                    Ok(section) => {
                        if tracks.is_none() {
                            tracks = self.tracks(&section.key, &mut report).await;
                        }
                        let path = row.path.clone().unwrap_or_default();
                        match tracks
                            .as_deref()
                            .and_then(|t| album_with(t, &row.audio_files))
                        {
                            Some((album, artist)) => {
                                row.stage = Stage::Ready;
                                row.plex_album = album;
                                row.plex_artist = artist;
                                row.problem = None;
                                row.problem_detail = None;
                                row.updated_at = now;
                            }
                            None if tracks.is_some() => {
                                let finished = row.finished_at.unwrap_or(now);
                                if now - finished > GIVE_UP_ON_PLEX_MS {
                                    row.stage = Stage::Failed;
                                    row.problem = Some(Problem::NotInPlex);
                                    row.problem_detail = Some(format!(
                                        "Plex never added the files in {path}; no longer followed"
                                    ));
                                    row.updated_at = now;
                                } else if row.scanned_at.is_none_or(|at| now - at > RESCAN_EVERY_MS)
                                {
                                    self.scan(&section.key, &path).await;
                                    row.scanned_at = Some(now);
                                    row.updated_at = now;
                                }
                                if now - finished > PLEX_GRACE_MS
                                    && !row.alerted.contains(&Problem::NotInPlex)
                                {
                                    row.problem = Some(Problem::NotInPlex);
                                    row.problem_detail = Some(format!(
                                        "finished 2 hours ago, but Plex has no album from {path}"
                                    ));
                                    row.updated_at = now;
                                    self.save(&row).await?;
                                    self.alert(&mut row, Problem::NotInPlex, &mut report)
                                        .await?;
                                }
                            }
                            None => {}
                        }
                    }
                }
            }
            if row != current {
                self.save(&row).await?;
            }
            if let Some(problem) = alert {
                self.alert(&mut row, problem, &mut report).await?;
            }
            if row.stage == Stage::Ready && row.ready_notice.is_none() {
                self.notify_ready(&mut row, &mut report).await?;
            }
        }
        Ok(report)
    }

    async fn tracks(&self, key: &str, report: &mut WatchReport) -> Option<Vec<PlexTrack>> {
        let plex = self.plex.as_ref().ok()?;
        match plex.fetch_music_tracks(key).await {
            Ok(tracks) => Some(tracks),
            Err(error) => {
                let reason = crate::arr::inspect::redact(error.cause_message());
                tracing::warn!(target: LOG, "Audiobook watch could not read Plex: {reason}");
                report.degraded = Some(reason);
                None
            }
        }
    }

    async fn scan(&self, key: &str, path: &str) {
        match self.plex_scan(key, path).await {
            Ok(()) => tracing::info!(target: LOG, "Asked Plex to scan {path}"),
            Err(reason) => tracing::warn!(target: LOG, "Plex scan of {path} failed: {reason}"),
        }
    }

    async fn save(&self, row: &AudiobookGrabData) -> Result<(), StoreError> {
        let row = row.clone();
        self.store.write(move |tx| tx.upsert(&row, ttl())).await
    }

    /// Records the problem as alerted, then pushes once.
    async fn alert(
        &self,
        row: &mut AudiobookGrabData,
        problem: Problem,
        report: &mut WatchReport,
    ) -> Result<(), StoreError> {
        if row.alerted.contains(&problem) {
            return Ok(());
        }
        row.alerted.push(problem);
        self.save(row).await?;
        report.alerts += 1;
        let detail = row.problem_detail.clone().unwrap_or_default();
        let message = format!("{}: {detail}.", row.display());
        tracing::warn!(target: LOG, "Audiobook needs attention: {message}");
        if let Err(error) = self
            .pushover
            .send(
                PushoverChannel::General,
                PushoverMessage {
                    title: Some("Audiobook needs attention".to_owned()),
                    message,
                    thread_id: Some("audiobooks".to_owned()),
                    ..PushoverMessage::default()
                },
            )
            .await
        {
            tracing::warn!(target: LOG, "Audiobook problem push failed: {error}");
        }
        Ok(())
    }

    /// Reserves, sends and settles the one "ready" notification.
    async fn notify_ready(
        &self,
        row: &mut AudiobookGrabData,
        report: &mut WatchReport,
    ) -> Result<(), StoreError> {
        row.ready_notice = Some(Notice::Sending);
        self.save(row).await?;
        let book = match (&row.plex_album, &row.plex_artist) {
            (Some(album), Some(artist)) => format!("{album} by {artist}"),
            _ => row.display(),
        };
        let sent = self
            .pushover
            .send(
                PushoverChannel::General,
                PushoverMessage {
                    title: Some("Audiobook ready".to_owned()),
                    message: format!("{book} is in your Plex audiobooks and ready in Prologue."),
                    thread_id: Some("audiobooks".to_owned()),
                    ..PushoverMessage::default()
                },
            )
            .await;
        row.ready_notice = Some(match &sent {
            Ok(_) => Notice::Sent,
            Err(error) if error.is_definite_rejection() => Notice::Failed,
            Err(_) => Notice::Uncertain,
        });
        if let Err(error) = &sent {
            tracing::warn!(target: LOG, "Audiobook ready push failed: {error}");
        }
        tracing::info!(target: LOG, "Audiobook ready: {book}");
        report.ready += 1;
        self.save(row).await
    }
}

/// Runs [`Audiobooks::watch`] every five minutes.
pub struct AudiobookWatchTask {
    books: Audiobooks,
    clock: omni_core::clock::SharedClock,
    schedule: CronSchedule,
}

impl AudiobookWatchTask {
    /// `None` (logged) when Prowlarr, Deluge or Plex is not configured.
    pub fn create(
        books: Audiobooks,
        clock: omni_core::clock::SharedClock,
        tz: &jiff::tz::TimeZone,
    ) -> Option<Self> {
        if !books.is_configured() {
            tracing::info!(
                target: "Main",
                "Audiobook watch disabled: Prowlarr, Deluge or Plex is not configured"
            );
            return None;
        }
        let schedule = CronSchedule::parse("0 */5 * * * *", tz).ok()?;
        Some(Self {
            books,
            clock,
            schedule,
        })
    }
}

impl Task for AudiobookWatchTask {
    fn name(&self) -> &str {
        TASK_NAME
    }

    fn display_name(&self) -> Option<&str> {
        Some("Audiobook Watch")
    }

    fn schedule(&self) -> &CronSchedule {
        &self.schedule
    }

    fn options(&self) -> TaskOptions {
        TaskOptions {
            jitter: std::time::Duration::ZERO,
            run_on_startup: true,
        }
    }

    fn run<'a>(&'a self, _cx: &'a RunContext) -> BoxFuture<'a, Result<(), TaskError>> {
        Box::pin(async move {
            let report = self
                .books
                .watch(self.clock.now_ms())
                .await
                .map_err(TaskError::from_error)?;
            if let Some(reason) = report.degraded {
                omni_tasks::report_degraded(reason);
            }
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use omni_store::cbor::Extra;

    fn row(stage: Stage) -> AudiobookGrabData {
        AudiobookGrabData {
            info_hash: "ab".repeat(20),
            release_id: "r".to_owned(),
            title: "Project Hail Mary by Andy Weir [ENG / M4B]".to_owned(),
            book: Some("Project Hail Mary".to_owned()),
            author: Some("Andy Weir".to_owned()),
            indexer: None,
            stage,
            requested_at: 0,
            updated_at: 0,
            progress_percent: None,
            progress_at: Some(0),
            finished_at: None,
            path: None,
            scanned_at: None,
            plex_album: None,
            plex_artist: None,
            problem: None,
            problem_detail: None,
            ready_notice: None,
            alerted: Vec::new(),
            audio_files: Vec::new(),
            missing_since: None,
            extra: Extra::default(),
        }
    }

    fn status(progress: f64, finished: bool, files: &[&str]) -> TorrentStatus {
        TorrentStatus {
            name: "Project Hail Mary".to_owned(),
            state: if finished { "Seeding" } else { "Downloading" }.to_owned(),
            progress_percent: progress,
            save_path: if finished { "/books" } else { "/incomplete" }.to_owned(),
            move_completed_path: "/books".to_owned(),
            is_finished: finished,
            files: files.iter().map(|f| (*f).to_owned()).collect(),
            ..TorrentStatus::default()
        }
    }

    #[test]
    fn a_moved_download_with_audio_is_scanned_where_it_landed() {
        let done = status(100.0, true, &["Project Hail Mary/Project Hail Mary.m4b"]);
        let d = decide(&row(Stage::Downloading), Observed::Present(&done), 1000);
        assert_eq!(d.row.stage, Stage::Finished);
        assert_eq!(d.scan.as_deref(), Some("/books/Project Hail Mary"));
        assert_eq!(d.alert, None);
    }

    #[test]
    fn finished_but_not_yet_moved_keeps_waiting() {
        let mut s = status(100.0, true, &["a.m4b"]);
        s.save_path = "/incomplete".to_owned();
        let d = decide(&row(Stage::Downloading), Observed::Present(&s), 1000);
        assert_eq!(d.row.stage, Stage::Downloading);
        assert_eq!(d.scan, None);
    }

    #[test]
    fn a_download_without_audio_fails_once() {
        let ebook = status(100.0, true, &["Book/book.epub"]);
        let d = decide(&row(Stage::Downloading), Observed::Present(&ebook), 1000);
        assert_eq!(d.row.stage, Stage::Failed);
        assert_eq!(d.alert, Some(Problem::NoAudio));
    }

    #[test]
    fn no_progress_for_twelve_hours_is_a_stall_alerted_once() {
        let mut current = row(Stage::Downloading);
        current.progress_percent = Some(37.0);
        let stuck = status(37.0, false, &["a.m4b"]);
        let later = STALL_AFTER_MS + 1;
        let first = decide(&current, Observed::Present(&stuck), later);
        assert_eq!(first.alert, Some(Problem::Stalled));
        let mut alerted = first.row;
        alerted.alerted.push(Problem::Stalled);
        let again = decide(&alerted, Observed::Present(&stuck), later + 1);
        assert_eq!(again.alert, None);
        let moving = status(40.0, false, &["a.m4b"]);
        let resumed = decide(&alerted, Observed::Present(&moving), later + 2);
        assert_eq!(resumed.row.problem, None);
    }

    #[test]
    fn an_uncertain_add_settles_only_by_reading_deluge() {
        let current = row(Stage::Uncertain);
        let seen = status(1.0, false, &["a.m4b"]);
        assert_eq!(
            decide(&current, Observed::Present(&seen), 1).row.stage,
            Stage::Downloading
        );
        assert_eq!(
            decide(&current, Observed::Absent, 1).row.stage,
            Stage::Uncertain
        );
        let gave_up = decide(&current, Observed::Absent, NEVER_ADDED_AFTER_MS + 1);
        assert_eq!(gave_up.row.stage, Stage::Failed);
        assert_eq!(gave_up.alert, Some(Problem::NeverAdded));
        assert_eq!(
            decide(&current, Observed::Unknown, NEVER_ADDED_AFTER_MS + 1).row,
            current
        );
    }

    #[test]
    fn a_torrent_is_missing_only_after_it_stays_absent() {
        let first = decide(&row(Stage::Downloading), Observed::Absent, 5);
        assert_eq!(first.row.stage, Stage::Downloading);
        assert_eq!(first.alert, None);
        let back = status(10.0, false, &["a.m4b"]);
        let revived = decide(&first.row, Observed::Present(&back), 6);
        assert_eq!(revived.row.missing_since, None);
        let soon = decide(&first.row, Observed::Absent, 6);
        assert_eq!(soon.row.stage, Stage::Downloading);
        let gone = decide(&first.row, Observed::Absent, 5 + MISSING_AFTER_MS);
        assert_eq!(gone.row.stage, Stage::Failed);
        assert_eq!(gone.alert, Some(Problem::Missing));
    }

    fn track(file: &str) -> PlexTrack {
        PlexTrack {
            album_rating_key: Some("1".to_owned()),
            album: Some("#58: The Ties That Bind".to_owned()),
            artist: Some("Adventures in Odyssey".to_owned()),
            files: vec![file.to_owned()],
        }
    }

    #[test]
    fn ready_needs_every_audio_file_in_plex_despite_punctuation_differences() {
        let files = vec![
            "/books/#58: The Ties that Bind/01 Part 1.mp3".to_owned(),
            "/books/#58: The Ties that Bind/02 Part 2.mp3".to_owned(),
        ];
        let first = track("/books/#58꞉ The Ties that Bind/01 Part 1.mp3");
        let second = track("/books/#58꞉ The Ties that Bind/02 Part 2.mp3");
        assert_eq!(album_with(std::slice::from_ref(&first), &files), None);
        assert_eq!(
            album_with(&[first, second], &files),
            Some((
                Some("#58: The Ties That Bind".to_owned()),
                Some("Adventures in Odyssey".to_owned())
            ))
        );
        assert_eq!(album_with(&[track("/books/x.mp3")], &[]), None);
    }
}
