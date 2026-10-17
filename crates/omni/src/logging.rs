//! The tracing subscriber stack (see AGENTS.md, Rust conventions, "Logging").
//!
//! Every layer filters on its own (`Layer::with_filter` semantics, implemented
//! inside each layer), never globally: the run-log layer must keep DEBUG lines
//! and run spans even when `LOG_LEVEL=info`.
//!
//! - console: `HH:mm:ss.mmm [LEVEL] <target> msg`, debug/info to stdout and
//!   warn/error to stderr, at `LOG_LEVEL`;
//! - optional daily file under `LOGS_PATH` (`omni-YYYY-MM-DD.log`, local
//!   date, 14-day retention swept on the first write);
//! - [`RunLogLayer`] (unfiltered; attribution by span);
//! - [`AlertLayer`] (ERROR only, gated and throttled by its worker).

use std::collections::HashSet;
use std::fmt::Write as _;
use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use jiff::tz::TimeZone;
use omni_alerts::AlertLayer;
use omni_core::LogLevel;
use omni_core::clock::SharedClock;
use omni_tasks::RunLogs;
use omni_tasks::log_capture::{RunLogLayer, is_app_event};
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::{Context, SubscriberExt as _};
use tracing_subscriber::registry::LookupSpan;

/// File retention (`retainDays`).
pub const FILE_RETAIN_DAYS: i64 = 14;
const LEVEL_PAD: usize = 5;
const NAME_PAD: usize = 16;

fn ordinal(level: LogLevel) -> u8 {
    match level {
        LogLevel::Debug => 0,
        LogLevel::Info => 1,
        LogLevel::Warn => 2,
        LogLevel::Error => 3,
    }
}

/// Whether the console and file sinks show an event: application events at
/// or above `threshold`, plus dependency warnings and errors.
pub fn shows(metadata: &tracing::Metadata<'_>, threshold: LogLevel) -> bool {
    let level = metadata.level();
    if *level == Level::TRACE {
        return false;
    }
    let event_level = LogLevel::from(level);
    if ordinal(event_level) < ordinal(threshold) {
        return false;
    }
    is_app_event(metadata) || ordinal(event_level) >= ordinal(LogLevel::Warn)
}

#[derive(Default)]
struct LineVisitor {
    message: String,
    fields: String,
}

impl Visit for LineVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message.push_str(value);
        } else {
            let _ = write!(self.fields, " {}={value}", field.name());
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            let _ = write!(self.message, "{value:?}");
        } else {
            let _ = write!(self.fields, " {}={value:?}", field.name());
        }
    }
}

fn event_text(event: &Event<'_>) -> String {
    let mut visitor = LineVisitor::default();
    event.record(&mut visitor);
    visitor.message.push_str(&visitor.fields);
    visitor.message
}

fn console_prefix(level: LogLevel) -> &'static str {
    match level {
        LogLevel::Debug => "[DEBUG]",
        LogLevel::Info => " [INFO]",
        LogLevel::Warn => " [WARN]",
        LogLevel::Error => "[ERROR]",
    }
}

/// `HH:mm:ss.mmm [LEVEL] <name> message` (UTC time).
pub fn format_console_line(ms: i64, level: LogLevel, name: &str, text: &str) -> String {
    let iso = omni_core::js::to_iso_string(ms);
    let time = iso.get(11..23).unwrap_or(&iso);
    format!("{time} {} <{name}> {text}", console_prefix(level))
}

/// Control bytes other than tab and newline become U+FFFD.
pub fn sanitize_control(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            '\t' | '\n' => c,
            '\0'..='\u{1f}' | '\u{7f}' => '\u{fffd}',
            _ => c,
        })
        .collect()
}

/// `<ISO> <LEVEL> <name> <message>`, continuation lines indented.
pub fn format_file_line(ms: i64, level: LogLevel, name: &str, text: &str) -> String {
    let level = format!(
        "{:<width$}",
        level.as_str().to_ascii_uppercase(),
        width = LEVEL_PAD
    );
    let name = format!("{name:<width$}", width = NAME_PAD);
    let text = sanitize_control(text);
    let body = if text.contains('\n') {
        text.replace('\n', "\n        ")
    } else {
        text
    };
    format!("{} {level} {name} {body}", omni_core::js::to_iso_string(ms))
}

/// Console sink at `LOG_LEVEL`.
pub struct ConsoleLayer {
    threshold: LogLevel,
    clock: SharedClock,
}

impl ConsoleLayer {
    pub fn new(threshold: LogLevel, clock: SharedClock) -> Self {
        Self { threshold, clock }
    }
}

impl<S: Subscriber> tracing_subscriber::Layer<S> for ConsoleLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let metadata = event.metadata();
        if !shows(metadata, self.threshold) {
            return;
        }
        let level = LogLevel::from(metadata.level());
        let line = format_console_line(
            self.clock.now_ms(),
            level,
            metadata.target(),
            &event_text(event),
        );
        // A closed stdout/stderr must never take the process down.
        let _ = if ordinal(level) >= ordinal(LogLevel::Warn) {
            writeln!(std::io::stderr().lock(), "{line}")
        } else {
            writeln!(std::io::stdout().lock(), "{line}")
        };
    }
}

struct FileState {
    initialized: bool,
    current: Option<(PathBuf, File)>,
    reported: HashSet<String>,
}

/// Daily file sink under `LOGS_PATH`.
pub struct DailyFileLayer {
    directory: PathBuf,
    threshold: LogLevel,
    tz: TimeZone,
    clock: SharedClock,
    state: Mutex<FileState>,
}

impl DailyFileLayer {
    pub fn new(directory: PathBuf, threshold: LogLevel, tz: TimeZone, clock: SharedClock) -> Self {
        Self {
            directory,
            threshold,
            tz,
            clock,
            state: Mutex::new(FileState {
                initialized: false,
                current: None,
                reported: HashSet::new(),
            }),
        }
    }

    /// `<prefix>-YYYY-MM-DD.log` for the local date of `ms`.
    pub fn file_name(&self, ms: i64) -> String {
        let date = omni_core::clock::timestamp_from_ms(ms)
            .to_zoned(self.tz.clone())
            .strftime("%Y-%m-%d")
            .to_string();
        format!("omni-{date}.log")
    }

    fn report(state: &mut FileState, what: &str, error: &dyn std::fmt::Display) {
        if state.reported.insert(what.to_owned()) {
            let _ = writeln!(
                std::io::stderr().lock(),
                "Logger file sink: {what}: {error}"
            );
        }
    }

    fn write_line(&self, ms: i64, line: &str) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !state.initialized {
            state.initialized = true;
            match std::fs::create_dir_all(&self.directory) {
                Ok(()) => sweep_old(&self.directory, ms, FILE_RETAIN_DAYS),
                Err(error) => {
                    let what = format!("mkdir {}", self.directory.display());
                    Self::report(&mut state, &what, &error);
                }
            }
        }
        let path = self.directory.join(self.file_name(ms));
        let reopen = state.current.as_ref().is_none_or(|(open, _)| *open != path);
        if reopen {
            match OpenOptions::new().create(true).append(true).open(&path) {
                Ok(file) => state.current = Some((path.clone(), file)),
                Err(error) => {
                    let what = format!("append {}", path.display());
                    Self::report(&mut state, &what, &error);
                    state.current = None;
                    return;
                }
            }
        }
        let failed = match state.current.as_mut() {
            Some((_, file)) => file.write_all(format!("{line}\n").as_bytes()).err(),
            None => None,
        };
        if let Some(error) = failed {
            let what = format!("append {}", path.display());
            Self::report(&mut state, &what, &error);
            state.current = None;
        }
    }
}

/// Deletes `omni-*.log` files whose mtime is older than `retain_days`.
pub fn sweep_old(directory: &Path, now_ms: i64, retain_days: i64) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    let cutoff_ms = now_ms - retain_days * 24 * 60 * 60 * 1000;
    let Ok(cutoff_ms) = u64::try_from(cutoff_ms) else {
        return;
    };
    let cutoff = SystemTime::UNIX_EPOCH + Duration::from_millis(cutoff_ms);
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("omni-") || !name.ends_with(".log") {
            continue;
        }
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .is_ok_and(|modified| modified < cutoff);
        if old {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

impl<S: Subscriber> tracing_subscriber::Layer<S> for DailyFileLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let metadata = event.metadata();
        if !shows(metadata, self.threshold) {
            return;
        }
        let ms = self.clock.now_ms();
        let line = format_file_line(
            ms,
            LogLevel::from(metadata.level()),
            metadata.target(),
            &event_text(event),
        );
        self.write_line(ms, &line);
    }
}

/// What the subscriber needs at install time.
pub struct LoggingSetup {
    pub level: LogLevel,
    pub logs_path: Option<PathBuf>,
    pub tz: TimeZone,
    pub clock: SharedClock,
}

/// The full stack over `S`, for installing globally or scoping in tests.
pub fn subscriber(
    setup: LoggingSetup,
    run_logs: Option<RunLogs>,
    alerts: Option<AlertLayer>,
) -> impl Subscriber + Send + Sync + for<'a> LookupSpan<'a> {
    let file = setup
        .logs_path
        .map(|dir| DailyFileLayer::new(dir, setup.level, setup.tz.clone(), setup.clock.clone()));
    tracing_subscriber::registry()
        .with(ConsoleLayer::new(setup.level, setup.clock.clone()))
        .with(file)
        .with(run_logs.map(RunLogLayer::new))
        .with(alerts)
}

/// Installs the stack as the global default.
pub fn install(
    setup: LoggingSetup,
    run_logs: Option<RunLogs>,
    alerts: Option<AlertLayer>,
) -> Result<(), tracing::subscriber::SetGlobalDefaultError> {
    tracing::subscriber::set_global_default(subscriber(setup, run_logs, alerts))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn console_line_format() {
        // 2026-10-09T12:34:56.789Z
        let ms = 1_791_549_296_789;
        assert_eq!(
            format_console_line(ms, LogLevel::Info, "Main:LiveCheck", "Went live"),
            "12:34:56.789  [INFO] <Main:LiveCheck> Went live"
        );
        assert_eq!(
            format_console_line(ms, LogLevel::Error, "Scheduler", "boom"),
            "12:34:56.789 [ERROR] <Scheduler> boom"
        );
    }

    #[test]
    fn file_line_pads_sanitizes_and_indents() {
        let ms = 1_791_549_296_789;
        assert_eq!(
            format_file_line(ms, LogLevel::Warn, "IMAP", "a\u{0}b\nnext"),
            "2026-10-09T12:34:56.789Z WARN  IMAP             a\u{fffd}b\n        next"
        );
    }

    #[test]
    fn dependency_debug_is_hidden_but_warnings_show() {
        let app = tracing::Metadata::new(
            "e",
            "LiveCheck",
            Level::DEBUG,
            None,
            None,
            None,
            tracing::field::FieldSet::new(&[], tracing::callsite::Identifier(&CALLSITE)),
            tracing::metadata::Kind::EVENT,
        );
        assert!(shows(&app, LogLevel::Debug));
        assert!(!shows(&app, LogLevel::Info));
        let dep_warn = tracing::Metadata::new(
            "e",
            "hyper::proto",
            Level::WARN,
            None,
            None,
            None,
            tracing::field::FieldSet::new(&[], tracing::callsite::Identifier(&CALLSITE)),
            tracing::metadata::Kind::EVENT,
        );
        assert!(shows(&dep_warn, LogLevel::Info));
        let dep_info = tracing::Metadata::new(
            "e",
            "hyper::proto",
            Level::INFO,
            None,
            None,
            None,
            tracing::field::FieldSet::new(&[], tracing::callsite::Identifier(&CALLSITE)),
            tracing::metadata::Kind::EVENT,
        );
        assert!(!shows(&dep_info, LogLevel::Info));
    }

    struct Callsite;
    static CALLSITE: Callsite = Callsite;
    impl tracing::callsite::Callsite for Callsite {
        fn set_interest(&self, _: tracing::subscriber::Interest) {}
        fn metadata(&self) -> &tracing::Metadata<'_> {
            unimplemented!()
        }
    }

    #[test]
    fn file_names_use_the_local_date() {
        let layer = DailyFileLayer::new(
            PathBuf::from("/tmp/x"),
            LogLevel::Info,
            TimeZone::get("America/Toronto").unwrap(),
            omni_core::clock::TestClock::new(0),
        );
        // 2026-10-09T02:00:00Z is still Oct 8 in Toronto.
        assert_eq!(layer.file_name(1_791_511_200_000), "omni-2026-10-08.log");
    }

    #[test]
    fn sweeps_only_old_omni_logs() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("omni-2020-01-01.log");
        let other = dir.path().join("other.log");
        std::fs::write(&old, "x").unwrap();
        std::fs::write(&other, "x").unwrap();
        let far_future = 4_102_444_800_000; // 2100
        sweep_old(dir.path(), far_future, FILE_RETAIN_DAYS);
        assert!(!old.exists());
        assert!(other.exists());
    }

    #[test]
    fn run_log_layer_keeps_debug_when_console_is_info() {
        let clock = omni_core::clock::TestClock::new(1_000);
        let bus = omni_tasks::EventBus::default();
        let logs = RunLogs::new(bus, clock.clone());
        logs.start("T:1", "T");
        let subscriber = subscriber(
            LoggingSetup {
                level: LogLevel::Error,
                logs_path: None,
                tz: TimeZone::UTC,
                clock,
            },
            Some(logs.clone()),
            None,
        );
        tracing::subscriber::with_default(subscriber, || {
            let span = omni_tasks::log_capture::run_span("T:1", "T");
            let _entered = span.enter();
            tracing::debug!(target: "T", "quiet detail");
        });
        let (lines, _) = logs.active("T:1").unwrap();
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].msg, "quiet detail");
    }
}
