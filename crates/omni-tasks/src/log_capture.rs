//! Run log capture as a tracing layer.
//!
//! A run is a span carrying a `run_id` field ([`run_span`], [`capture_scope`]).
//! Every event inside it, at any level and across `.instrument`ed async work,
//! lands in that run's bounded buffer and is broadcast as a
//! [`RunLogEvent::Line`](crate::RunLogEvent). Events outside a run are ignored.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::{Arc, Mutex, MutexGuard};

use omni_core::clock::SharedClock;
use omni_core::js::{utf16_len, utf16_slice};
use omni_store::LogLine;
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id};
use tracing::{Event, Subscriber};
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;

use crate::{EventBus, RunLogEvent};

/// Lines longer than this (UTF-16 units) are cut and suffixed with `…`.
pub const MAX_LINE_CHARS: usize = 32_768;
/// Per-run ring size; older lines are dropped and counted.
pub const MAX_LINES: usize = 20_000;

/// The run (or synthetic capture) a piece of code executes inside.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunAttribution {
    pub run_id: String,
    pub task_name: String,
}

#[derive(Debug, Default)]
struct RunLogBuffer {
    task_name: String,
    lines: Vec<LogLine>,
    next_line: usize,
    dropped: u64,
}

impl RunLogBuffer {
    fn push(&mut self, line: LogLine) {
        if self.lines.len() < MAX_LINES {
            self.lines.push(line);
        } else {
            self.lines[self.next_line] = line;
            self.next_line = (self.next_line + 1) % MAX_LINES;
            self.dropped += 1;
        }
    }

    fn ordered(&self) -> Vec<LogLine> {
        if self.lines.len() < MAX_LINES || self.next_line == 0 {
            return self.lines.clone();
        }
        let mut out = Vec::with_capacity(self.lines.len());
        out.extend_from_slice(&self.lines[self.next_line..]);
        out.extend_from_slice(&self.lines[..self.next_line]);
        out
    }
}

/// Live per-run buffers; cheap to clone.
#[derive(Clone)]
pub struct RunLogs {
    buffers: Arc<Mutex<HashMap<String, RunLogBuffer>>>,
    bus: EventBus,
    clock: SharedClock,
}

impl RunLogs {
    pub fn new(bus: EventBus, clock: SharedClock) -> Self {
        Self {
            buffers: Arc::new(Mutex::new(HashMap::new())),
            bus,
            clock,
        }
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<String, RunLogBuffer>> {
        self.buffers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Begins buffering lines for `run_id`.
    pub fn start(&self, run_id: &str, task_name: &str) {
        self.lock().insert(
            run_id.to_owned(),
            RunLogBuffer {
                task_name: task_name.to_owned(),
                ..RunLogBuffer::default()
            },
        );
    }

    /// Buffered lines (oldest first) and the dropped count of an in-flight run.
    pub fn active(&self, run_id: &str) -> Option<(Vec<LogLine>, u64)> {
        self.lock()
            .get(run_id)
            .map(|buffer| (buffer.ordered(), buffer.dropped))
    }

    /// Stops buffering and returns the task name, lines and dropped count.
    pub fn take(&self, run_id: &str) -> Option<(String, Vec<LogLine>, u64)> {
        self.lock()
            .remove(run_id)
            .map(|buffer| (buffer.task_name.clone(), buffer.ordered(), buffer.dropped))
    }

    fn record(&self, run_id: &str, line: LogLine) {
        let accepted = match self.lock().get_mut(run_id) {
            Some(buffer) => {
                buffer.push(line.clone());
                true
            }
            None => false,
        };
        if accepted {
            self.bus.emit_run_log(RunLogEvent::Line {
                run_id: run_id.to_owned(),
                line,
            });
        }
    }
}

/// Tracing layer that attributes events to the innermost span with a
/// `run_id` field. Install it without a level filter, and give the console
/// layer its `LOG_LEVEL` filter per layer (`Layer::with_filter`): a global
/// filter would disable DEBUG events and INFO run spans for this layer too.
/// Only [`is_app_event`] events are captured.
pub struct RunLogLayer {
    logs: RunLogs,
}

impl RunLogLayer {
    pub fn new(logs: RunLogs) -> Self {
        Self { logs }
    }
}

#[derive(Default)]
struct AttributionVisitor {
    run_id: Option<String>,
    task: Option<String>,
}

impl Visit for AttributionVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        match field.name() {
            "run_id" => self.run_id = Some(value.to_owned()),
            "task" => self.task = Some(value.to_owned()),
            _ => {}
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        match field.name() {
            "run_id" => self.run_id = Some(format!("{value:?}").trim_matches('"').to_owned()),
            "task" => self.task = Some(format!("{value:?}").trim_matches('"').to_owned()),
            _ => {}
        }
    }
}

#[derive(Default)]
struct MessageVisitor {
    message: String,
    fields: String,
}

impl Visit for MessageVisitor {
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

impl<S> tracing_subscriber::Layer<S> for RunLogLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let mut visitor = AttributionVisitor::default();
        attrs.record(&mut visitor);
        if let (Some(run_id), Some(span)) = (visitor.run_id, ctx.span(id)) {
            let task_name = visitor.task.unwrap_or_else(|| "Unknown".to_owned());
            span.extensions_mut()
                .insert(RunAttribution { run_id, task_name });
        }
    }

    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        if !is_app_event(event.metadata()) {
            return;
        }
        let Some(scope) = ctx.event_scope(event) else {
            return;
        };
        let Some(attribution) = scope
            .into_iter()
            .find_map(|span| span.extensions().get::<RunAttribution>().cloned())
        else {
            return;
        };
        let mut visitor = MessageVisitor::default();
        event.record(&mut visitor);
        let mut text = visitor.message;
        text.push_str(&visitor.fields);
        if utf16_len(&text) > MAX_LINE_CHARS {
            text = format!("{}…", utf16_slice(&text, 0, MAX_LINE_CHARS));
        }
        let metadata = event.metadata();
        let line = LogLine {
            t: self.logs.clock.now_ms(),
            level: metadata.level().into(),
            logger: metadata.target().to_owned(),
            msg: text,
        };
        self.logs.record(&attribution.run_id, line);
    }
}

/// Whether an event is application logging: DEBUG and above from a named logger target (`"LiveCheck"`,
/// `"Main:TaskRegistry"`) or an `omni_*` or `omni::` module path. TRACE events and
/// dependency internals (`hyper::proto::h1`, `rustls::client`) are not run
/// log lines.
pub fn is_app_event(metadata: &tracing::Metadata<'_>) -> bool {
    if *metadata.level() == tracing::Level::TRACE {
        return false;
    }
    let target = metadata.target();
    !target.contains("::") || target.starts_with("omni_") || target.starts_with("omni::")
}

/// `info_span!("task_run", run_id, task)`: everything inside is attributed to the run.
pub fn run_span(run_id: &str, task: &str) -> tracing::Span {
    tracing::info_span!("task_run", run_id = run_id, task = task)
}

/// An ad-hoc capture (per-email pipeline work) that returns its lines instead
/// of persisting them as a task run.
pub fn capture_scope(
    logs: &RunLogs,
    capture_id: &str,
    name: &str,
) -> (tracing::Span, CaptureHandle) {
    logs.start(capture_id, name);
    let span = tracing::info_span!("capture", run_id = capture_id, task = name);
    (
        span,
        CaptureHandle {
            logs: logs.clone(),
            capture_id: capture_id.to_owned(),
        },
    )
}

/// Ends a [`capture_scope`].
pub struct CaptureHandle {
    logs: RunLogs,
    capture_id: String,
}

impl CaptureHandle {
    /// Stops buffering; returns the lines (oldest first) and the dropped count.
    pub fn finish(self) -> (Vec<LogLine>, u64) {
        self.logs
            .take(&self.capture_id)
            .map(|(_, lines, dropped)| (lines, dropped))
            .unwrap_or_default()
    }
}

/// The run or capture the current span belongs to (cost attribution). Requires
/// the subscriber to be built on `tracing_subscriber::Registry`.
pub fn current_run() -> Option<RunAttribution> {
    tracing::Span::current()
        .with_subscriber(|(id, dispatch)| {
            let registry = dispatch.downcast_ref::<tracing_subscriber::Registry>()?;
            let span = registry.span(id)?;
            span.scope()
                .find_map(|span| span.extensions().get::<RunAttribution>().cloned())
        })
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;
    use omni_core::clock::TestClock;
    use tracing::Instrument as _;
    use tracing_subscriber::layer::SubscriberExt as _;

    #[tokio::test(start_paused = true)]
    async fn captures_events_inside_runs_only() {
        let bus = EventBus::new(16);
        let logs = RunLogs::new(bus.clone(), TestClock::new(42));
        let subscriber = tracing_subscriber::registry().with(RunLogLayer::new(logs.clone()));
        let _guard = tracing::subscriber::set_default(subscriber);

        tracing::info!(target: "Outside", "ignored");
        logs.start("Task:1", "Task");
        async {
            tracing::debug!(target: "LiveCheck", count = 3, "checked");
            assert_eq!(
                current_run(),
                Some(RunAttribution {
                    run_id: "Task:1".to_owned(),
                    task_name: "Task".to_owned()
                })
            );
        }
        .instrument(run_span("Task:1", "Task"))
        .await;

        let (lines, dropped) = logs.active("Task:1").unwrap_or_default();
        assert_eq!(dropped, 0);
        assert_eq!(
            lines,
            vec![LogLine {
                t: 42,
                level: omni_core::LogLevel::Debug,
                logger: "LiveCheck".to_owned(),
                msg: "checked count=3".to_owned(),
            }]
        );
        assert!(current_run().is_none());
    }

    #[test]
    fn ring_buffer_drops_oldest() {
        let mut buffer = RunLogBuffer::default();
        for t in 0..(MAX_LINES as i64 + 2) {
            buffer.push(LogLine {
                t,
                level: omni_core::LogLevel::Info,
                logger: "L".to_owned(),
                msg: String::new(),
            });
        }
        let ordered = buffer.ordered();
        assert_eq!(buffer.dropped, 2);
        assert_eq!(ordered.first().map(|l| l.t), Some(2));
        assert_eq!(ordered.last().map(|l| l.t), Some(MAX_LINES as i64 + 1));
    }

    #[test]
    fn capture_scope_returns_lines() {
        let logs = RunLogs::new(EventBus::new(4), TestClock::new(0));
        let subscriber = tracing_subscriber::registry().with(RunLogLayer::new(logs.clone()));
        let _guard = tracing::subscriber::set_default(subscriber);
        let (span, handle) = capture_scope(&logs, "email:1", "ParcelTracker");
        span.in_scope(|| tracing::warn!(target: "Parcel", "hello"));
        let (lines, dropped) = handle.finish();
        assert_eq!(dropped, 0);
        assert_eq!(lines.len(), 1);
        assert!(logs.active("email:1").is_none());
    }
}
