//! One `shell` call's run: the tool calls its script makes, their measurements
//! and the file cache.
//!
//! Every tool call goes through [`Run::call`], from a command or a file read.
//! Calls run the tool's own handler with the gateway's input projection, so
//! a script sees exactly what a direct call returns, printed as indented JSON.
//! A run allows at most [`MAX_TOOL_CALLS`] calls, [`CONCURRENT_CALLS`] at a
//! time, and stops them when the `shell` request goes away.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use futures::FutureExt as _;
use omni_core::clock::SharedClock;
use omni_mcp_kit::{ToolContext, ToolError, ToolOutput};
use serde_json::Value;
use tokio::sync::{OnceCell, Semaphore};
use tokio_util::sync::CancellationToken;

use super::commands::Command;
use crate::json::{apply_defaults, order_by_schema};

const LOG: &str = "MCP:Shell";

/// Tool calls one script may make.
pub(crate) const MAX_TOOL_CALLS: usize = 200;
/// Tool calls one script may have running at once (background jobs).
const CONCURRENT_CALLS: usize = 4;
/// Calls kept in a run's record; the count covers the rest.
pub(crate) const RECORDED_CALLS: usize = 50;

/// How a script reached a tool.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Via {
    Command,
    File,
}

impl Via {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Command => "command",
            Self::File => "file",
        }
    }
}

/// One tool call a script made.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CallRecord {
    pub tool: String,
    pub via: Via,
    pub duration_ms: i64,
    /// Bytes of the result as compact JSON; 0 for a failed call.
    pub bytes: i64,
    pub ok: bool,
}

#[derive(Debug, Default)]
pub(crate) struct Calls {
    pub count: usize,
    pub bytes: i64,
    /// Calls and result bytes per tool, over every call.
    pub by_tool: BTreeMap<String, (i64, i64)>,
    pub records: Vec<CallRecord>,
}

type FileCell = Arc<OnceCell<Result<Arc<String>, String>>>;

pub(crate) struct Run {
    clock: SharedClock,
    cancel: CancellationToken,
    permits: Semaphore,
    calls: Mutex<Calls>,
    files: Mutex<HashMap<String, FileCell>>,
}

impl Run {
    pub fn new(clock: SharedClock, cancel: CancellationToken) -> Self {
        Self {
            clock,
            cancel,
            permits: Semaphore::new(CONCURRENT_CALLS),
            calls: Mutex::new(Calls::default()),
            files: Mutex::new(HashMap::new()),
        }
    }

    /// The calls made so far.
    pub fn take_calls(&self) -> Calls {
        self.calls
            .lock()
            .map(|mut calls| std::mem::take(&mut *calls))
            .unwrap_or_default()
    }

    /// The tool's result for the run's first read of its file; later reads
    /// in the same run reuse it.
    pub async fn file(&self, command: &Command) -> Result<Arc<String>, String> {
        let cell = {
            let Ok(mut files) = self.files.lock() else {
                return Err("the file cache is unavailable".to_owned());
            };
            files.entry(command.name().to_owned()).or_default().clone()
        };
        cell.get_or_init(|| async {
            self.call(command, Value::Object(serde_json::Map::new()), Via::File)
                .await
                .map(Arc::new)
                .map_err(|error| error.message)
        })
        .await
        .clone()
    }

    /// Runs the tool and returns its result as indented JSON with a final newline.
    pub async fn call(
        &self,
        command: &Command,
        input: Value,
        via: Via,
    ) -> Result<String, ToolError> {
        let name = command.name().to_owned();
        {
            let mut calls = self
                .calls
                .lock()
                .map_err(|_| ToolError::execute("the run's call record is unavailable"))?;
            if calls.count >= MAX_TOOL_CALLS {
                return Err(ToolError::execute(format!(
                    "this script reached its limit of {MAX_TOOL_CALLS} tool calls"
                )));
            }
            calls.count += 1;
        }
        let _permit = self
            .permits
            .acquire()
            .await
            .map_err(|_| ToolError::execute("the run has ended"))?;
        let started = self.clock.now_ms();
        let input = apply_defaults(input, &command.input_schema);
        let context = ToolContext {
            call_id: omni_core::ids::uuid_v4(),
            cancel: self.cancel.child_token(),
        };
        let handler = command.tool.handler.clone();
        let outcome =
            std::panic::AssertUnwindSafe(async move { handler.call(input, context).await })
                .catch_unwind()
                .await
                .unwrap_or_else(|panic| {
                    let message = panic
                        .downcast_ref::<&str>()
                        .map(|s| (*s).to_owned())
                        .or_else(|| panic.downcast_ref::<String>().cloned())
                        .unwrap_or_else(|| "the tool panicked".to_owned());
                    tracing::error!(target: LOG, "Shell tool {name} panicked: {message}");
                    Err(ToolError::execute(message))
                });
        // Measured as the compact JSON a direct call returns, so shell and
        // direct sizes compare; printed indented for line-oriented tools.
        let mut compact = 0;
        let result = outcome.map(|output| {
            let structured = match output {
                ToolOutput::Structured(map)
                | ToolOutput::Custom {
                    structured: map, ..
                } => map,
            };
            let value = omni_core::js::normalize_numbers(Value::Object(structured));
            let value = order_by_schema(value, &command.output_schema);
            compact = omni_core::js::json_stringify(&value).len();
            let mut text = serde_json::to_string_pretty(&value).unwrap_or_default();
            text.push('\n');
            text
        });
        let record = CallRecord {
            tool: name,
            via,
            duration_ms: self.clock.now_ms() - started,
            bytes: i64::try_from(compact).unwrap_or(i64::MAX),
            ok: result.is_ok(),
        };
        if let Ok(mut calls) = self.calls.lock() {
            calls.bytes += record.bytes;
            let entry = calls.by_tool.entry(record.tool.clone()).or_default();
            entry.0 += 1;
            entry.1 += record.bytes;
            if calls.records.len() < RECORDED_CALLS {
                calls.records.push(record);
            }
        }
        result
    }
}
