//! Agent-initiated push notifications to the owner (`notify_owner_*` tools).
//!
//! Each call reserves its idempotency key as an `owner-notification` row
//! (`sending`) before the push and records `sent` or `recorded` after it. The
//! same key never sends twice: a repeat returns the stored outcome, and an
//! uncertain push (no APNs answer, a Pushover timeout or 5xx) keeps its
//! `sending` row and is never retried automatically. A definite rejection
//! releases the row so the caller can retry with the same key. Reservations
//! are also the rate limit: at most [`HOURLY_LIMIT`] per rolling hour and
//! [`DAILY_LIMIT`] per rolling day across every key.

use omni_alerts::{PushOutcome, Pushover, PushoverChannel, PushoverMessage};
use omni_core::clock::SharedClock;
use omni_core::js::to_iso_string;
use omni_store::cbor::Extra;
use omni_store::entity::{Entity, UpsertOpts};
use omni_store::{EntityOps, EntityWrite, Store, StoreError};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio_util::task::TaskTracker;

const LOG: &str = "OwnerNotify";

/// Reservations (and so key dedup) last 30 days.
pub const RESERVATION_TTL_MS: i64 = 30 * DAY_MS;
/// At most this many notifications per rolling hour.
pub const HOURLY_LIMIT: usize = 10;
/// At most this many notifications per rolling 24 hours.
pub const DAILY_LIMIT: usize = 40;
/// Pushover's message limit, in characters.
pub const MAX_MESSAGE_CHARS: usize = 1024;
/// Pushover's supplementary URL limit, in characters.
pub const MAX_URL_CHARS: usize = 512;

const HOUR_MS: i64 = 60 * 60 * 1000;
const DAY_MS: i64 = 24 * HOUR_MS;

/// Why the agent is notifying the owner: `Blocked` on something only the
/// owner can do, or `Requested` because the owner asked to be told.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum NotificationKind {
    Blocked,
    Requested,
}

/// The push's priority: never emergency or anything that needs acknowledgement.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum NotificationPriority {
    #[default]
    Normal,
    High,
}

impl NotificationPriority {
    fn pushover(self) -> Option<i8> {
        match self {
            NotificationPriority::Normal => None,
            NotificationPriority::High => Some(1),
        }
    }
}

/// A reservation's delivery state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReservationStatus {
    /// Reserved before the push; stays so when the outcome is unknown.
    Sending,
    Sent,
    /// `SideEffectMode::Record`: captured, not sent.
    Recorded,
}

/// `owner-notification`, keyed by the caller's idempotency key. Holds no
/// message text, only a digest of the composed push.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OwnerNotification {
    pub key: String,
    pub kind: NotificationKind,
    pub fingerprint: String,
    pub status: ReservationStatus,
    pub reserved_at: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sent_at: Option<f64>,
    pub updated_at: f64,
    #[serde(flatten)]
    pub extra: Extra,
}

impl Entity for OwnerNotification {
    const NAME: &'static str = "owner-notification";
    const DEFAULT_TTL_MS: Option<i64> = Some(RESERVATION_TTL_MS);
    type Key = String;
    fn key(&self) -> String {
        self.key.clone()
    }
}

#[allow(clippy::cast_precision_loss)]
fn ms_f(value: i64) -> f64 {
    value as f64
}

#[allow(clippy::cast_possible_truncation)]
fn ms_i(value: f64) -> i64 {
    value as i64
}

/// A composed push, before reservation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OwnerPush {
    pub kind: NotificationKind,
    pub title: String,
    pub message: String,
    pub url: Option<String>,
    pub url_title: Option<String>,
    pub priority: NotificationPriority,
}

impl OwnerPush {
    /// A stable digest of everything the owner would see.
    fn fingerprint(&self) -> String {
        let mut hasher = Sha256::new();
        let priority = match self.priority {
            NotificationPriority::Normal => "normal",
            NotificationPriority::High => "high",
        };
        let kind = match self.kind {
            NotificationKind::Blocked => "blocked",
            NotificationKind::Requested => "requested",
        };
        for part in [
            kind,
            &self.title,
            &self.message,
            self.url.as_deref().unwrap_or(""),
            self.url_title.as_deref().unwrap_or(""),
            priority,
        ] {
            hasher.update(part.as_bytes());
            hasher.update([0]);
        }
        hex::encode(hasher.finalize())
    }

    fn message(&self) -> PushoverMessage {
        PushoverMessage {
            message: self.message.clone(),
            title: Some(self.title.clone()),
            url: self.url.clone(),
            url_title: self
                .url
                .as_ref()
                .map(|_| self.url_title.clone().unwrap_or_else(|| "Open".to_owned())),
            priority: self.priority.pushover(),
            ..PushoverMessage::default()
        }
    }
}

/// A trimmed, non-empty single line, or an input error naming `field`.
pub fn clean_line(field: &str, value: &str) -> Result<String, OwnerNotifyError> {
    let collapsed = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        return Err(OwnerNotifyError::Input(format!(
            "{field} must not be blank"
        )));
    }
    Ok(collapsed)
}

/// Trimmed text that may keep line breaks, or an input error naming `field`.
pub fn clean_text(field: &str, value: &str) -> Result<String, OwnerNotifyError> {
    let lines: Vec<String> = value
        .lines()
        .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
        .collect();
    let text = lines.join("\n").trim().to_owned();
    if text.is_empty() {
        return Err(OwnerNotifyError::Input(format!(
            "{field} must not be blank"
        )));
    }
    Ok(text)
}

/// An absolute `https://` URL within Pushover's length limit.
pub fn https_url(value: &str) -> Result<String, OwnerNotifyError> {
    let trimmed = value.trim();
    let invalid = || OwnerNotifyError::Input("url must be an absolute https:// URL".to_owned());
    if trimmed.chars().count() > MAX_URL_CHARS {
        return Err(OwnerNotifyError::Input(format!(
            "url must be at most {MAX_URL_CHARS} characters"
        )));
    }
    let parsed = url::Url::parse(trimmed).map_err(|_| invalid())?;
    if parsed.scheme() != "https" || parsed.host_str().is_none_or(str::is_empty) {
        return Err(invalid());
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(OwnerNotifyError::Input(
            "url must not carry credentials".to_owned(),
        ));
    }
    Ok(trimmed.to_owned())
}

/// Strips a leading `1.` / `2)` the caller may have numbered a step with.
fn strip_numbering(step: &str) -> &str {
    let digits = step.chars().take_while(char::is_ascii_digit).count();
    if digits == 0 {
        return step;
    }
    let rest = &step[digits..];
    match rest.strip_prefix('.').or_else(|| rest.strip_prefix(')')) {
        Some(after) if after.starts_with(' ') => after.trim_start(),
        _ => step,
    }
}

/// The fields of a blocker notification, as the tool receives them.
pub struct BlockedFields<'a> {
    pub task: &'a str,
    pub blocker: &'a str,
    pub steps: &'a [String],
    pub from_phone: bool,
    pub meanwhile: Option<&'a str>,
}

/// `Blocked: <task>` with the blocker, numbered steps and what the agent does
/// meanwhile.
pub fn compose_blocked(fields: &BlockedFields<'_>) -> Result<(String, String), OwnerNotifyError> {
    let task = clean_line("task", fields.task)?;
    let blocker = clean_text("blocker", fields.blocker)?;
    if fields.steps.is_empty() {
        return Err(OwnerNotifyError::Input(
            "steps must name at least one action".to_owned(),
        ));
    }
    let mut message = blocker;
    message.push_str(if fields.from_phone {
        "\n\nFrom your phone:"
    } else {
        "\n\nNeeds a computer:"
    });
    for (index, step) in fields.steps.iter().enumerate() {
        let step = clean_line("steps", step)?;
        let step = clean_line("steps", strip_numbering(&step))?;
        message.push_str(&format!("\n{}. {step}", index + 1));
    }
    if let Some(meanwhile) = fields.meanwhile {
        message.push_str("\n\nMeanwhile: ");
        message.push_str(&clean_line("meanwhile", meanwhile)?);
    }
    Ok((format!("Blocked: {task}"), message))
}

/// The headline as the title, the message, and the owner's request it answers.
pub fn compose_requested(
    request: &str,
    title: &str,
    message: &str,
) -> Result<(String, String), OwnerNotifyError> {
    let request = clean_line("request", request)?;
    let title = clean_line("title", title)?;
    let mut body = clean_text("message", message)?;
    body.push_str(&format!("\n\nYou asked: \"{request}\""));
    Ok((title, body))
}

/// What one call did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeliveryState {
    Sent,
    Recorded,
    /// The push may or may not have arrived; it is never resent.
    Uncertain,
}

/// The settled outcome of a call (or of the first call with its key).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NotifyOutcome {
    pub state: DeliveryState,
    /// The key was already reserved; nothing new was sent.
    pub replayed: bool,
    pub kind: NotificationKind,
    pub key: String,
    pub reserved_at: i64,
    pub sent_at: Option<i64>,
    pub remaining_this_hour: usize,
    pub remaining_today: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum OwnerNotifyError {
    #[error("{0}")]
    Input(String),
    #[error(
        "idempotencyKey {key} was already used for a different notification; use a new key for a new notification"
    )]
    KeyReused { key: String },
    #[error(
        "Owner notification limit reached (at most {HOURLY_LIMIT} per hour and {DAILY_LIMIT} per day). The next one is allowed at {next_at}. Do not work around the limit: continue independent work or wait."
    )]
    RateLimited { next_at: String },
    #[error(
        "The notification was not delivered: {cause}. Nothing reached the owner; retrying with the same idempotencyKey is safe."
    )]
    Rejected { cause: String },
    #[error(transparent)]
    Store(#[from] StoreError),
}

enum Reservation {
    Reserved(OwnerNotification),
    Existing(OwnerNotification),
}

enum PushResult {
    Delivered(ReservationStatus),
    Rejected(String),
    Uncertain(String),
}

async fn deliver(pushover: &Pushover, push: &OwnerPush) -> PushResult {
    match pushover
        .send(PushoverChannel::General, push.message())
        .await
    {
        Ok(PushOutcome::Sent) => PushResult::Delivered(ReservationStatus::Sent),
        Ok(PushOutcome::Recorded) => PushResult::Delivered(ReservationStatus::Recorded),
        Ok(PushOutcome::SkippedNoToken | PushOutcome::Disabled) => {
            PushResult::Rejected("push notifications are not configured".to_owned())
        }
        Err(error) if error.is_definite_rejection() => PushResult::Rejected(error.to_string()),
        Err(error) => PushResult::Uncertain(error.to_string()),
    }
}

/// Reserves, sends and records owner notifications.
#[derive(Clone)]
pub struct OwnerNotifier {
    store: Store,
    clock: SharedClock,
    pushover: Pushover,
    tracker: TaskTracker,
}

impl OwnerNotifier {
    pub fn new(store: Store, clock: SharedClock, pushover: Pushover, tracker: TaskTracker) -> Self {
        Self {
            store,
            clock,
            pushover,
            tracker,
        }
    }

    /// Sends `push` once per `key`. The reservation, push and record run to
    /// completion even when the caller is dropped.
    pub async fn notify(
        &self,
        key: String,
        push: OwnerPush,
    ) -> Result<NotifyOutcome, OwnerNotifyError> {
        if push.message.chars().count() > MAX_MESSAGE_CHARS {
            return Err(OwnerNotifyError::Input(format!(
                "the composed message must be at most {MAX_MESSAGE_CHARS} characters; shorten it"
            )));
        }
        let this = self.clone();
        omni_core::spawn::must_complete(&self.tracker, async move { this.run(key, push).await })
            .await
    }

    async fn run(&self, key: String, push: OwnerPush) -> Result<NotifyOutcome, OwnerNotifyError> {
        let now = self.clock.now_ms();
        let row = match self.reserve(&key, &push, now).await? {
            Reservation::Existing(row) => return self.outcome(&row, true, now).await,
            Reservation::Reserved(row) => row,
        };
        match deliver(&self.pushover, &push).await {
            PushResult::Delivered(status) => {
                let sent_at = self.clock.now_ms();
                let mut done = row;
                done.status = status;
                done.sent_at = Some(ms_f(sent_at));
                done.updated_at = ms_f(sent_at);
                let saved = done.clone();
                self.store
                    .write(move |tx| tx.upsert(&saved, UpsertOpts::default()))
                    .await?;
                tracing::info!(
                    target: LOG,
                    kind = ?push.kind,
                    key = %key,
                    "Owner notification {}",
                    if status == ReservationStatus::Sent { "sent" } else { "recorded" }
                );
                self.outcome(&done, false, sent_at).await
            }
            PushResult::Rejected(cause) => {
                let release = key.clone();
                self.store
                    .write(move |tx| tx.delete::<OwnerNotification>(&release).map(|_| ()))
                    .await?;
                tracing::warn!(target: LOG, key = %key, "Owner notification rejected: {cause}");
                Err(OwnerNotifyError::Rejected { cause })
            }
            PushResult::Uncertain(cause) => {
                tracing::warn!(
                    target: LOG,
                    key = %key,
                    "Owner notification outcome unknown; not retrying: {cause}"
                );
                self.outcome(&row, false, now).await
            }
        }
    }

    async fn reserve(
        &self,
        key: &str,
        push: &OwnerPush,
        now: i64,
    ) -> Result<Reservation, OwnerNotifyError> {
        let key = key.to_owned();
        let kind = push.kind;
        let fingerprint = push.fingerprint();
        self.store
            .write(
                move |tx| -> Result<Result<Reservation, OwnerNotifyError>, StoreError> {
                    if let Some(existing) = tx.get::<OwnerNotification>(&key)? {
                        if existing.fingerprint != fingerprint || existing.kind != kind {
                            return Ok(Err(OwnerNotifyError::KeyReused { key }));
                        }
                        return Ok(Ok(Reservation::Existing(existing)));
                    }
                    let reserved: Vec<i64> = tx
                        .get_all::<OwnerNotification>()?
                        .iter()
                        .map(|row| ms_i(row.reserved_at))
                        .collect();
                    if let Some(next_at) = next_allowed(&reserved, now) {
                        return Ok(Err(OwnerNotifyError::RateLimited {
                            next_at: to_iso_string(next_at),
                        }));
                    }
                    let row = OwnerNotification {
                        key,
                        kind,
                        fingerprint,
                        status: ReservationStatus::Sending,
                        reserved_at: ms_f(now),
                        sent_at: None,
                        updated_at: ms_f(now),
                        extra: Extra::default(),
                    };
                    tx.upsert(&row, UpsertOpts::default())?;
                    Ok(Ok(Reservation::Reserved(row)))
                },
            )
            .await?
    }

    async fn outcome(
        &self,
        row: &OwnerNotification,
        replayed: bool,
        now: i64,
    ) -> Result<NotifyOutcome, OwnerNotifyError> {
        let reserved: Vec<i64> = self
            .store
            .read(|docs| docs.get_all::<OwnerNotification>())
            .await?
            .iter()
            .map(|r| ms_i(r.reserved_at))
            .collect();
        let (hour, day) = window_counts(&reserved, now);
        Ok(NotifyOutcome {
            state: match row.status {
                ReservationStatus::Sending => DeliveryState::Uncertain,
                ReservationStatus::Sent => DeliveryState::Sent,
                ReservationStatus::Recorded => DeliveryState::Recorded,
            },
            replayed,
            kind: row.kind,
            key: row.key.clone(),
            reserved_at: ms_i(row.reserved_at),
            sent_at: row.sent_at.map(ms_i),
            remaining_this_hour: HOURLY_LIMIT.saturating_sub(hour),
            remaining_today: DAILY_LIMIT.saturating_sub(day),
        })
    }
}

/// Reservations inside the rolling hour and day ending at `now`.
fn window_counts(reserved: &[i64], now: i64) -> (usize, usize) {
    let hour = reserved.iter().filter(|at| now - **at < HOUR_MS).count();
    let day = reserved.iter().filter(|at| now - **at < DAY_MS).count();
    (hour, day)
}

/// `None` when another notification is allowed at `now`; otherwise the
/// earliest time both windows have room again.
fn next_allowed(reserved: &[i64], now: i64) -> Option<i64> {
    let mut in_hour: Vec<i64> = reserved
        .iter()
        .copied()
        .filter(|at| now - at < HOUR_MS)
        .collect();
    let mut in_day: Vec<i64> = reserved
        .iter()
        .copied()
        .filter(|at| now - at < DAY_MS)
        .collect();
    in_hour.sort_unstable();
    in_day.sort_unstable();
    let mut next: Option<i64> = None;
    if in_hour.len() >= HOURLY_LIMIT {
        // The oldest reservation that must age out to bring the count below the limit.
        let at = in_hour[in_hour.len() - HOURLY_LIMIT] + HOUR_MS;
        next = Some(next.map_or(at, |n: i64| n.max(at)));
    }
    if in_day.len() >= DAILY_LIMIT {
        let at = in_day[in_day.len() - DAILY_LIMIT] + DAY_MS;
        next = Some(next.map_or(at, |n: i64| n.max(at)));
    }
    next
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_limit_opens_when_enough_reservations_age_out() {
        let now = 10 * DAY_MS;
        assert_eq!(next_allowed(&[], now), None);
        let hourly = HOURLY_LIMIT as i64;
        let under: Vec<i64> = (1..hourly).map(|i| now - i * 10).collect();
        assert_eq!(next_allowed(&under, now), None);
        let at: Vec<i64> = (1..=hourly).map(|i| now - i * 10).collect();
        assert_eq!(next_allowed(&at, now), Some(now - hourly * 10 + HOUR_MS));
        // A full day spread so that no hour reaches the hourly limit.
        let daily = DAILY_LIMIT as i64;
        let spacing = DAY_MS / (daily + 1);
        let day: Vec<i64> = (1..=daily).map(|i| now - i * spacing).collect();
        assert_eq!(
            next_allowed(&day, now),
            Some(now - daily * spacing + DAY_MS)
        );
    }

    #[test]
    fn blocked_messages_number_steps_and_name_the_device() {
        let steps = vec!["1. Open the Omni app".to_owned(), "Tap  Approve".to_owned()];
        let (title, message) = compose_blocked(&BlockedFields {
            task: " Omni deploy ",
            blocker: "The deploy needs approval.",
            steps: &steps,
            from_phone: true,
            meanwhile: Some("Running tests"),
        })
        .unwrap();
        assert_eq!(title, "Blocked: Omni deploy");
        assert_eq!(
            message,
            "The deploy needs approval.\n\nFrom your phone:\n1. Open the Omni app\n2. Tap Approve\n\nMeanwhile: Running tests"
        );
    }

    #[test]
    fn urls_must_be_https_without_credentials() {
        assert!(https_url("https://example.com/a?b=1").is_ok());
        for bad in [
            "http://example.com",
            "example.com",
            "https://user:pw@example.com",
            "javascript:alert(1)",
        ] {
            assert!(
                matches!(https_url(bad), Err(OwnerNotifyError::Input(_))),
                "{bad}"
            );
        }
    }
}
