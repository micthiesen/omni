//! Builds every subsystem, sets every port, assembles the email pipeline and
//! wires the background services.

use std::collections::HashMap;
use std::sync::Arc;

use omni_core::email::{EmailHandler, EmailLinkMetadata};
use omni_email::link_metadata::{HeaderLine, ParsedMailView};
use omni_imap::{BodyEnricher, LinkMetadataInput};
use omni_runtime::ports::EmailRetryHandlers;
use omni_runtime::{AppContext, Subsystem};

const LOG: &str = "Main";

/// A subsystem that could not be assembled (fails boot).
#[derive(Debug, thiserror::Error)]
#[error("{subsystem}: {message}")]
pub struct WiringError {
    pub subsystem: &'static str,
    pub message: String,
}

fn fail(subsystem: &'static str) -> impl FnOnce(&dyn std::fmt::Display) -> WiringError {
    move |e| WiringError {
        subsystem,
        message: e.to_string(),
    }
}

macro_rules! wire {
    ($name:literal, $expr:expr) => {
        $expr.map_err(|e| fail($name)(&e))?
    };
}

/// omni-email's body rendering for the IMAP transport (HTML to text and link
/// metadata).
pub struct EmailBodyEnricher;

/// mailsplit `_decodeHeaderValue`: UTF-8 when valid, else latin1.
fn header_text(raw: &[u8]) -> String {
    match std::str::from_utf8(raw) {
        Ok(text) => text.to_owned(),
        Err(_) => raw.iter().map(|&b| char::from(b)).collect(),
    }
}

impl BodyEnricher for EmailBodyEnricher {
    fn html_to_text(&self, html: &str) -> String {
        omni_email::html_to_text::html_to_text(html)
    }

    fn interesting_links(&self, html: &str) -> Vec<String> {
        omni_email::html_to_text::extract_interesting_links(html)
    }

    fn link_metadata(&self, input: LinkMetadataInput<'_>) -> EmailLinkMetadata {
        let header_lines: Vec<HeaderLine> = input
            .header_lines
            .iter()
            .map(|line| HeaderLine {
                key: line.key.clone(),
                line: header_text(&line.raw),
            })
            .collect();
        omni_email::link_metadata::extract_email_link_metadata(ParsedMailView {
            html: input.html,
            text: input.text,
            header_lines: &header_lines,
        })
    }
}

/// `EmailRetryHandlers`: the pipelines retry and reprocess can re-run
/// (ParcelTracker, CalendarEvents; never McpEvents).
pub struct RetryHandlers(HashMap<String, Arc<dyn EmailHandler>>);

impl RetryHandlers {
    pub fn new(handlers: &[Arc<dyn EmailHandler>]) -> Self {
        Self(
            handlers
                .iter()
                .map(|h| (h.name().to_owned(), h.clone()))
                .collect(),
        )
    }
}

impl EmailRetryHandlers for RetryHandlers {
    fn handler(&self, pipeline: &str) -> Option<Arc<dyn EmailHandler>> {
        self.0.get(pipeline).cloned()
    }
}

/// Tasks that only exist with an email transport (iCloud credentials set).
const EMAIL_TASKS: &[&str] = &["EmailArchive", "EmailWatchdog", "EmailRetry"];

/// The wired application.
pub struct Wired {
    /// Every subsystem in registration order; the MCP subsystem is last.
    pub subsystems: Vec<Subsystem>,
    /// Ordered email handlers (McpEvents, ParcelTracker, CalendarEvents)
    /// when a transport is configured.
    pub email_handlers: Vec<Arc<dyn EmailHandler>>,
}

fn take_handlers(subsystem: &mut Subsystem) -> Vec<Arc<dyn EmailHandler>> {
    std::mem::take(&mut subsystem.email_handlers)
}

/// Builds every subsystem over `ctx` and sets every port. `booted_at` is the
/// process start (the email watchdog's first-dispatch stand-in).
pub async fn wire(ctx: &AppContext, booted_at: i64) -> Result<Wired, WiringError> {
    let ports = &ctx.ports;
    let mut subsystems = Vec::new();

    // Livestreams: intelligence (port set by its boot step), stored streamers
    // (loaded by a boot step), iOS controls reconciled after every tick.
    subsystems.push(omni_live_intel::subsystem(ctx));
    for warning in omni_config::legacy_warnings(&std::env::vars().collect()) {
        tracing::warn!(target: LOG, "{warning}");
    }
    let live = wire!("live", omni_live::LiveModule::load(ctx));
    let ios = omni_ios_controls::IosControls::new(ctx, live.roster()).await;
    // Every notification (ERROR-log alerts included) tries the app over APNs
    // before Pushover; the Pushover client is shared, so this covers all clones.
    if ctx.pushover.set_app_route(ios.app_route()).is_err() {
        tracing::warn!(target: LOG, "iOS app notification route was already set");
    }
    subsystems.push(wire!("live", live.into_subsystem(Some(ios.reconciler()))));
    subsystems.push(ios.into_subsystem());

    subsystems.push(wire!("personal", omni_personal::subsystem(ctx).await));
    subsystems.push(wire!("presspods", omni_presspods::subsystem(ctx)));
    subsystems.push(wire!("media", omni_media::subsystem(ctx)));
    subsystems.push(omni_arr::subsystem(ctx));
    subsystems.push(wire!("podcasts", omni_podcasts::subsystem(ctx)));
    subsystems.push(wire!("reminders", omni_reminders::subsystem(ctx)));
    subsystems.push(wire!("stylist", omni_stylist::subsystem(ctx)));

    // Email: one shared triage for parcel and calendar.
    let (imap, handles) = wire!(
        "imap",
        omni_imap::subsystem(ctx, Arc::new(EmailBodyEnricher), None)
    );
    if let Some(reader) = handles.email_reader() {
        let _ = ports.set_email_reader(reader);
    }
    let _ = ports.set_archive_echo(handles.archive_echo());
    let triage = omni_email::triage::EmailTriage::with_model(
        ctx.ai.clone(),
        ctx.config.clone(),
        ctx.store.clone(),
    );
    let _ = ports.set_deliveries_source(omni_parcel::deliveries_source(ctx));
    let mut parcel = wire!("parcel", omni_parcel::subsystem(ctx, triage.clone()));
    let parcel_handlers = take_handlers(&mut parcel);
    let _ = ports.set_calendar_connection(omni_calendar::calendar_connection(ctx));
    let mut calendar = wire!(
        "calendar",
        omni_calendar::subsystem(ctx, omni_calendar::CalendarDeps::new(ctx, triage))
    );
    let calendar_handlers = take_handlers(&mut calendar);
    let mut email = wire!("email", omni_email::subsystem(ctx, booted_at));
    let mail_source = handles.mail_source();
    if mail_source.is_none() {
        email.tasks.retain(|t| !EMAIL_TASKS.contains(&t.name()));
    }
    subsystems.push(imap);
    subsystems.push(parcel);
    subsystems.push(calendar);
    subsystems.push(email);

    // Claude Code host before MCP: building the device link sets its port.
    let device_link = omni_device_link::DeviceLink::from_context(ctx);
    if let Some(link) = &device_link {
        subsystems.push(link.subsystem());
    }
    let mcp = wire!("mcp", omni_mcp::McpPackage::new(ctx));
    if let Some(watcher) = mcp.watcher() {
        let _ = ports.set_claude_session_notifier(Arc::new(watcher.clone()));
    }
    // Subsystems built above read this port when they publish, not at build time.
    let _ = ports.set_event_publisher(Arc::new(mcp.event_publisher()));

    let retry_handlers: Vec<Arc<dyn EmailHandler>> = parcel_handlers
        .iter()
        .chain(&calendar_handlers)
        .cloned()
        .collect();
    let mut email_handlers = Vec::new();
    if let Some(source) = mail_source {
        email_handlers.push(mcp.email_handler());
        email_handlers.extend(retry_handlers.iter().cloned());
        let _ = ports.set_email_retry_handlers(Arc::new(RetryHandlers::new(&retry_handlers)));
        let mut dispatcher = Subsystem::named("email-dispatcher");
        dispatcher.services.push(omni_email::dispatcher::service(
            source,
            "IMAP",
            email_handlers.clone(),
        ));
        subsystems.push(dispatcher);
    } else {
        let _ = ports.set_email_retry_handlers(Arc::new(RetryHandlers::new(&[])));
    }

    // MCP last: it serves every package's tools in golden order.
    let mut tools = Vec::new();
    for subsystem in &mut subsystems {
        tools.append(&mut subsystem.mcp_tools);
    }
    tools.extend(wire!("mcp", mcp.tools()));
    subsystems.push(wire!("mcp", mcp.subsystem(tools)));

    Ok(Wired {
        subsystems,
        email_handlers,
    })
}

/// Registration order: internal tasks, then the scheduled feature tasks, then
/// the email tasks. Names not listed sort last.
pub const TASK_ORDER: &[&str] = &[
    "McpEventDelivery",
    "ClaudeSessionEvents",
    "TaskRunEvents",
    "RemindersSession",
    "CalendarPrimarySync",
    "CalendarStartingEvents",
    "LiveCheckTask",
    "StreamerAvatars",
    "PetTracker",
    "PressPods",
    "Recommendations",
    "ArrRecovery",
    "ObserverRepair",
    "PodcastRecs",
    "CastroInboxCleanup",
    "TasteReflection",
    "PodcastTasteReflection",
    "CodexResets",
    "ClaudeResets",
    "ParcelDeliveries",
    "StylistBackup",
    "EmailArchive",
    "EmailWatchdog",
    "EmailRetry",
];

/// Tasks `--run-task` may run (`buildTasks`; the internal and email tasks are
/// registered only by the full boot).
pub fn runnable_from_cli(name: &str) -> bool {
    ![
        "McpEventDelivery",
        "ClaudeSessionEvents",
        "TaskRunEvents",
        "RemindersSession",
        "CalendarStartingEvents",
        "EmailArchive",
        "EmailWatchdog",
        "EmailRetry",
    ]
    .contains(&name)
}

/// Sort rank for [`TASK_ORDER`].
pub fn task_rank(name: &str) -> usize {
    TASK_ORDER
        .iter()
        .position(|n| *n == name)
        .unwrap_or(TASK_ORDER.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unlisted_tasks_sort_last() {
        let mut names = vec!["EmailRetry", "Unlisted", "PressPods", "McpEventDelivery"];
        names.sort_by_key(|n| task_rank(n));
        assert_eq!(
            names,
            vec!["McpEventDelivery", "PressPods", "EmailRetry", "Unlisted"]
        );
    }

    #[test]
    fn header_lines_decode_like_mailsplit() {
        assert_eq!(header_text(b"Subject: hi"), "Subject: hi");
        assert_eq!(header_text(&[0x61, 0xe9]), "a\u{e9}");
    }
}
