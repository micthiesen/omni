//! MCP call recording.
//!
//! Every tool call that passes input validation is recorded as `running`, then
//! finished as `ok`, `error` or `interrupted` (the request went away). Inputs
//! are bounded and secret-looking keys redacted; outputs are kept only for
//! `claude_*` tools, whose results are the activity. Recording failures are
//! logged and never change the tool's own result.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use indexmap::IndexMap;
use omni_api::mcp_activity::{
    McpActivityResponse, McpActivitySummary, McpCall, McpCallStatus, McpRetention, McpToolSummary,
    RecommendedPolicy,
};
use omni_core::clock::SharedClock;
use omni_mcp_kit::{ApprovalPolicy, ToolMeta};
use omni_store::cbor::{Extra, JsValue};
use omni_store::entity::{Entity, EntityOps, EntityWrite, ModifyOpts, UpsertOpts};
use omni_store::{Store, StoreError};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tokio_util::task::TaskTracker;

use crate::events::persistence::{js_to_json, json_to_js};

const LOG: &str = "MCP:Activity";
pub const MCP_ACTIVITY_MAX_CALLS: usize = 2_000;
const PRUNE_SLACK: usize = 100;
const DEFAULT_STRING_LIMIT: usize = 300;
const CLAUDE_STRING_LIMIT: usize = 4_000;
const ARRAY_LIMIT: usize = 20;
const DEPTH_LIMIT: usize = 6;
const ERROR_LIMIT: usize = 2_000;
const DAY_MS: i64 = 24 * 60 * 60 * 1000;

/// `mcp-call`, keyed by `callId`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpCallData {
    pub call_id: String,
    pub tool: String,
    pub title: String,
    pub recommended_policy: RecommendedPolicy,
    pub read_only: bool,
    pub started_at: i64,
    pub finished_at: Option<i64>,
    pub duration_ms: Option<i64>,
    pub status: McpCallStatus,
    pub error: Option<String>,
    #[serde(default = "undefined")]
    pub input: JsValue,
    /// Recorded only for Claude session tools.
    #[serde(default = "undefined")]
    pub output: JsValue,
    /// Bytes of the result's text content, what a client passes to its model
    /// (absent before sizes were recorded, and for failed calls).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_bytes: Option<i64>,
    /// Bytes of the result's `structuredContent` as JSON.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structured_bytes: Option<i64>,
    #[serde(flatten)]
    pub extra: Extra,
}

fn undefined() -> JsValue {
    JsValue::Undefined
}

impl Entity for McpCallData {
    const NAME: &'static str = "mcp-call";
    type Key = String;
    fn key(&self) -> String {
        self.call_id.clone()
    }
}

impl McpCallData {
    fn to_view(&self) -> McpCall {
        McpCall {
            call_id: self.call_id.clone(),
            tool: self.tool.clone(),
            title: self.title.clone(),
            recommended_policy: self.recommended_policy,
            read_only: self.read_only,
            started_at: self.started_at,
            finished_at: self.finished_at,
            duration_ms: self.duration_ms,
            status: self.status,
            error: self.error.clone(),
            input: js_to_json(&self.input),
            output: js_to_json(&self.output),
        }
    }
}

pub fn is_claude_tool(tool: &str) -> bool {
    tool.starts_with("claude_")
}

fn string_limit_for(tool: &str) -> usize {
    if is_claude_tool(tool) {
        CLAUDE_STRING_LIMIT
    } else {
        DEFAULT_STRING_LIMIT
    }
}

fn truncate_text(value: &str, limit: usize) -> String {
    let length = omni_core::js::utf16_len(value);
    if length > limit {
        format!(
            "{}… [truncated {} chars]",
            omni_core::js::utf16_slice(value, 0, limit),
            length - limit
        )
    } else {
        value.to_owned()
    }
}

/// Identifier fields kept from a `stylist_*` call; everything else is personal
/// (body measurements, notes, photos) and recorded only as `[private]`.
const STYLIST_ACTIVITY_KEYS: [&str; 17] = [
    "namespace",
    "idempotencyKey",
    "id",
    "expectedRevision",
    "supersedes",
    "supersedesEventId",
    "collection",
    "kinds",
    "ids",
    "linkedTo",
    "evidenceId",
    "archiveId",
    "asOfRevision",
    "cursor",
    "limit",
    "mimeType",
    "occurredOn",
];

/// The input copy kept in activity: stylist calls keep only identifiers and
/// the record kind or event type.
pub fn activity_input(tool: &str, input: &Value) -> Value {
    if !tool.starts_with("stylist_") {
        return input.clone();
    }
    let Value::Object(map) = input else {
        return Value::String("[private]".to_owned());
    };
    let mut kept = Map::new();
    for (key, value) in map {
        let entry = if STYLIST_ACTIVITY_KEYS.contains(&key.as_str()) {
            value.clone()
        } else if key == "record" || key == "event" {
            let mut summary = Map::new();
            for tag in ["kind", "type"] {
                if let Some(tag_value) = value.get(tag) {
                    summary.insert(tag.to_owned(), tag_value.clone());
                }
            }
            Value::Object(summary)
        } else {
            Value::String("[private]".to_owned())
        };
        kept.insert(key.clone(), entry);
    }
    Value::Object(kept)
}

/// The error text kept in activity: for stylist calls only the `[code]`
/// prefix, since a rejection can echo a measurement or an order.
pub fn activity_error(tool: &str, message: String) -> String {
    if !tool.starts_with("stylist_") {
        return message;
    }
    match message.split_once(']') {
        Some((code, _)) if code.starts_with('[') => format!("{code}]"),
        _ => "[error]".to_owned(),
    }
}

/// `/token|secret|password|passwd|authorization|api[_-]?key|cookie/i`.
fn is_secret_key(key: &str) -> bool {
    let lower = key.to_lowercase();
    [
        "token",
        "secret",
        "password",
        "passwd",
        "authorization",
        "apikey",
        "api_key",
        "api-key",
        "cookie",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

/// Caps strings, arrays and depth; hides secret-looking keys.
pub fn bound_value(value: &Value, string_limit: usize) -> Value {
    bound_at(value, string_limit, 0)
}

fn bound_at(value: &Value, string_limit: usize, depth: usize) -> Value {
    match value {
        Value::String(s) => Value::String(truncate_text(s, string_limit)),
        Value::Array(_) | Value::Object(_) if depth >= DEPTH_LIMIT => {
            Value::String("[nested]".to_owned())
        }
        Value::Array(items) => {
            let mut bounded: Vec<Value> = items
                .iter()
                .take(ARRAY_LIMIT)
                .map(|item| bound_at(item, string_limit, depth + 1))
                .collect();
            if items.len() > ARRAY_LIMIT {
                bounded.push(Value::String(format!(
                    "… [{} more items]",
                    items.len() - ARRAY_LIMIT
                )));
            }
            Value::Array(bounded)
        }
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, entry)| {
                    let bounded = if is_secret_key(key) {
                        Value::String("[redacted]".to_owned())
                    } else {
                        bound_at(entry, string_limit, depth + 1)
                    };
                    (key.clone(), bounded)
                })
                .collect(),
        ),
        other => other.clone(),
    }
}

fn policy_of(meta: &ToolMeta) -> RecommendedPolicy {
    match meta.policy.recommended_policy {
        ApprovalPolicy::Allow => RecommendedPolicy::Allow,
        ApprovalPolicy::RequireApproval => RecommendedPolicy::RequireApproval,
        ApprovalPolicy::Block => RecommendedPolicy::Block,
    }
}

/// How a recorded call ended.
#[derive(Clone, Debug, PartialEq)]
pub enum CallOutcome {
    Ok(Option<Map<String, Value>>),
    Error(String),
    Interrupted,
}

struct RecorderInner {
    store: Store,
    clock: SharedClock,
    tracker: TaskTracker,
    since_prune: AtomicUsize,
}

/// Records MCP calls; cheap to clone.
#[derive(Clone)]
pub struct ActivityRecorder {
    inner: Arc<RecorderInner>,
}

/// A call recorded as `running`. Dropping it unfinished (the request was
/// cancelled) records `interrupted` on the app tracker.
pub struct RecordedCall {
    recorder: ActivityRecorder,
    call_id: String,
    tool: String,
    started_at: i64,
    size: Option<ResultSize>,
    finished: bool,
}

/// The size of a successful `tools/call` result, in UTF-8 bytes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ResultSize {
    /// Every text content block.
    pub content: i64,
    /// `structuredContent` serialized as JSON.
    pub structured: i64,
}

impl ResultSize {
    pub fn of(result: &Map<String, Value>) -> Self {
        let len = |n: usize| i64::try_from(n).unwrap_or(i64::MAX);
        let content = result
            .get("content")
            .and_then(Value::as_array)
            .map_or(0, |blocks| {
                blocks
                    .iter()
                    .filter_map(|block| block.get("text").and_then(Value::as_str))
                    .map(|text| len(text.len()))
                    .sum()
            });
        let structured = result
            .get("structuredContent")
            .map_or(0, |value| len(omni_core::js::json_stringify(value).len()));
        Self {
            content,
            structured,
        }
    }
}

impl ActivityRecorder {
    pub fn new(store: Store, clock: SharedClock, tracker: TaskTracker) -> Self {
        Self {
            inner: Arc::new(RecorderInner {
                store,
                clock,
                tracker,
                since_prune: AtomicUsize::new(0),
            }),
        }
    }

    pub async fn start(&self, meta: &ToolMeta, input: &Value) -> RecordedCall {
        let call_id = omni_core::ids::uuid_v4();
        let started_at = self.inner.clock.now_ms();
        let row = McpCallData {
            call_id: call_id.clone(),
            tool: meta.name.clone(),
            title: meta.title.clone(),
            recommended_policy: policy_of(meta),
            read_only: meta.annotations.read_only_hint,
            started_at,
            finished_at: None,
            duration_ms: None,
            status: McpCallStatus::Running,
            error: None,
            input: json_to_js(&bound_value(
                &activity_input(&meta.name, input),
                string_limit_for(&meta.name),
            )),
            output: JsValue::Null,
            result_bytes: None,
            structured_bytes: None,
            extra: Extra::default(),
        };
        if let Err(error) = self
            .inner
            .store
            .write(move |tx| tx.upsert(&row, UpsertOpts::default()))
            .await
        {
            tracing::warn!(target: LOG, "MCP activity start failed for {}: {error}", meta.name);
        }
        RecordedCall {
            recorder: self.clone(),
            call_id,
            tool: meta.name.clone(),
            started_at,
            size: None,
            finished: false,
        }
    }

    async fn finish(
        &self,
        call_id: String,
        tool: String,
        started_at: i64,
        outcome: CallOutcome,
        size: Option<ResultSize>,
    ) {
        let finished_at = self.inner.clock.now_ms();
        let (status, error, output) = match outcome {
            CallOutcome::Ok(output) => (McpCallStatus::Ok, None, output),
            CallOutcome::Error(message) => (McpCallStatus::Error, Some(message), None),
            CallOutcome::Interrupted => (
                McpCallStatus::Interrupted,
                Some("The MCP request was cancelled".to_owned()),
                None,
            ),
        };
        let error = error.map(|message| activity_error(&tool, message));
        let output = match output {
            Some(output) if is_claude_tool(&tool) => json_to_js(&bound_value(
                &Value::Object(output),
                string_limit_for(&tool),
            )),
            _ => JsValue::Null,
        };
        let mut patch: IndexMap<String, JsValue> = IndexMap::new();
        patch.insert("finishedAt".into(), JsValue::Int(finished_at.into()));
        patch.insert(
            "durationMs".into(),
            JsValue::Int((finished_at - started_at).into()),
        );
        patch.insert("status".into(), status_js(status));
        patch.insert(
            "error".into(),
            error.map_or(JsValue::Null, |e| {
                JsValue::String(truncate_text(&e, ERROR_LIMIT))
            }),
        );
        patch.insert("output".into(), output);
        if let (Some(size), McpCallStatus::Ok) = (size, status) {
            patch.insert("resultBytes".into(), JsValue::Int(size.content.into()));
            patch.insert(
                "structuredBytes".into(),
                JsValue::Int(size.structured.into()),
            );
        }
        let key = call_id;
        if let Err(error) = self
            .inner
            .store
            .write(move |tx| {
                tx.patch::<McpCallData>(&key, patch, ModifyOpts::default())
                    .map(|_| ())
            })
            .await
        {
            tracing::warn!(target: LOG, "MCP activity finish failed for {tool}: {error}");
        }
        if self.inner.since_prune.fetch_add(1, Ordering::Relaxed) + 1 >= PRUNE_SLACK {
            self.inner.since_prune.store(0, Ordering::Relaxed);
            if let Err(error) = prune_calls(&self.inner.store).await {
                tracing::warn!(target: LOG, "MCP activity prune failed for {tool}: {error}");
            }
        }
    }
}

fn status_js(status: McpCallStatus) -> JsValue {
    JsValue::String(
        match status {
            McpCallStatus::Running => "running",
            McpCallStatus::Ok => "ok",
            McpCallStatus::Error => "error",
            McpCallStatus::Interrupted => "interrupted",
        }
        .to_owned(),
    )
}

impl RecordedCall {
    /// Records the result's size with a successful finish.
    pub fn sized(mut self, result: &Map<String, Value>) -> Self {
        self.size = Some(ResultSize::of(result));
        self
    }

    pub async fn finish(mut self, outcome: CallOutcome) {
        self.finished = true;
        let call_id = std::mem::take(&mut self.call_id);
        let tool = std::mem::take(&mut self.tool);
        self.recorder
            .finish(call_id, tool, self.started_at, outcome, self.size)
            .await;
    }
}

impl RecordedCall {
    /// Forgets a call that never ran (its arguments failed validation).
    pub async fn discard(mut self) {
        self.finished = true;
        let key = std::mem::take(&mut self.call_id);
        let tool = std::mem::take(&mut self.tool);
        if let Err(error) = self
            .recorder
            .inner
            .store
            .write(move |tx| tx.delete::<McpCallData>(&key).map(|_| ()))
            .await
        {
            tracing::warn!(target: LOG, "MCP activity discard failed for {tool}: {error}");
        }
    }
}

impl Drop for RecordedCall {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        let recorder = self.recorder.clone();
        let call_id = std::mem::take(&mut self.call_id);
        let tool = std::mem::take(&mut self.tool);
        let started_at = self.started_at;
        let tracker = recorder.inner.tracker.clone();
        omni_core::spawn::spawn_tracked(&tracker, "mcp-call-interrupted", async move {
            recorder
                .finish(call_id, tool, started_at, CallOutcome::Interrupted, None)
                .await;
        });
    }
}

/// Keeps the newest [`MCP_ACTIVITY_MAX_CALLS`] once the count exceeds it by the slack.
pub async fn prune_calls(store: &Store) -> Result<usize, StoreError> {
    store
        .write(|tx| {
            if tx.count::<McpCallData>()? <= (MCP_ACTIVITY_MAX_CALLS + PRUNE_SLACK) as u64 {
                return Ok(0);
            }
            let mut calls = tx.get_all::<McpCallData>()?;
            calls.sort_by_key(|call| std::cmp::Reverse(call.started_at));
            let stale: Vec<String> = calls
                .into_iter()
                .skip(MCP_ACTIVITY_MAX_CALLS)
                .map(|call| call.call_id)
                .collect();
            for id in &stale {
                tx.delete::<McpCallData>(id)?;
            }
            Ok::<_, StoreError>(stale.len())
        })
        .await
}

/// `markInterruptedCalls`: calls left running by a restarted process can never
/// finish. Runs at boot.
pub async fn mark_interrupted_calls(store: &Store, now: i64) -> Result<usize, StoreError> {
    let marked = store
        .write(move |tx| {
            let running: Vec<McpCallData> = tx
                .get_all::<McpCallData>()?
                .into_iter()
                .filter(|call| call.status == McpCallStatus::Running)
                .collect();
            for call in &running {
                let mut patch: IndexMap<String, JsValue> = IndexMap::new();
                patch.insert("status".into(), status_js(McpCallStatus::Interrupted));
                patch.insert(
                    "error".into(),
                    JsValue::String("Omni restarted before the call finished".into()),
                );
                patch.insert("finishedAt".into(), JsValue::Int(now.into()));
                patch.insert(
                    "durationMs".into(),
                    JsValue::Int((now - call.started_at).into()),
                );
                tx.patch::<McpCallData>(&call.call_id, patch, ModifyOpts::default())?;
            }
            Ok::<_, StoreError>(running.len())
        })
        .await?;
    prune_calls(store).await?;
    Ok(marked)
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ActivityQuery {
    pub limit: usize,
    pub tool: Option<String>,
    pub status: Option<McpCallStatus>,
    pub before: Option<i64>,
    pub tool_prefix: Option<String>,
}

/// The pure view over stored calls for the activity pages.
pub fn summarize_calls(
    stored: &[McpCallData],
    query: &ActivityQuery,
    now: i64,
) -> McpActivityResponse {
    let mut newest_first: Vec<&McpCallData> = stored.iter().collect();
    newest_first.sort_by_key(|call| std::cmp::Reverse(call.started_at));
    let scoped: Vec<&McpCallData> = match &query.tool_prefix {
        Some(prefix) if !prefix.is_empty() => newest_first
            .into_iter()
            .filter(|call| call.tool.starts_with(prefix.as_str()))
            .collect(),
        _ => newest_first,
    };
    let matching: Vec<&McpCallData> = scoped
        .iter()
        .copied()
        .filter(|call| {
            query
                .tool
                .as_deref()
                .is_none_or(|t| t.is_empty() || call.tool == t)
                && query.status.is_none_or(|s| call.status == s)
                && query.before.is_none_or(|b| call.started_at < b)
        })
        .collect();
    let calls: Vec<&McpCallData> = matching.iter().copied().take(query.limit).collect();
    let next_before = (matching.len() > calls.len())
        .then(|| calls.last().map(|call| call.started_at))
        .flatten();

    let recent: Vec<&McpCallData> = scoped
        .iter()
        .copied()
        .filter(|call| now - call.started_at < DAY_MS)
        .collect();
    struct Acc {
        summary: McpToolSummary,
        duration_total: i64,
        timed: i64,
    }
    let mut order: Vec<String> = Vec::new();
    let mut by_tool: HashMap<String, Acc> = HashMap::new();
    for call in &scoped {
        let entry = by_tool.entry(call.tool.clone()).or_insert_with(|| {
            order.push(call.tool.clone());
            Acc {
                summary: McpToolSummary {
                    tool: call.tool.clone(),
                    title: call.title.clone(),
                    calls: 0,
                    errors: 0,
                    last_at: call.started_at,
                    avg_duration_ms: None,
                    recommended_policy: call.recommended_policy,
                },
                duration_total: 0,
                timed: 0,
            }
        });
        entry.summary.calls += 1;
        if call.status.is_failure() {
            entry.summary.errors += 1;
        }
        entry.summary.last_at = entry.summary.last_at.max(call.started_at);
        if let Some(duration) = call.duration_ms {
            entry.duration_total += duration;
            entry.timed += 1;
        }
    }
    let mut tools: Vec<McpToolSummary> = order
        .into_iter()
        .filter_map(|tool| by_tool.remove(&tool))
        .map(|acc| McpToolSummary {
            #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
            avg_duration_ms: (acc.timed > 0)
                .then(|| (acc.duration_total as f64 / acc.timed as f64).round() as i64),
            ..acc.summary
        })
        .collect();
    tools.sort_by_key(|tool| std::cmp::Reverse(tool.last_at));
    let count = |calls: &[&McpCallData], f: &dyn Fn(&McpCallData) -> bool| {
        calls.iter().filter(|c| f(c)).count() as u64
    };
    McpActivityResponse {
        calls: calls.into_iter().map(McpCallData::to_view).collect(),
        next_before,
        summary: McpActivitySummary {
            stored: scoped.len() as u64,
            last24h: recent.len() as u64,
            errors24h: count(&recent, &|c| c.status.is_failure()),
            running: count(&scoped, &|c| c.status == McpCallStatus::Running),
            approval_calls24h: count(&recent, &|c| {
                c.recommended_policy == RecommendedPolicy::RequireApproval
            }),
        },
        tools,
        retention: McpRetention {
            max_calls: MCP_ACTIVITY_MAX_CALLS as u64,
        },
    }
}

pub async fn get_mcp_activity(
    store: &Store,
    query: &ActivityQuery,
    now: i64,
) -> Result<McpActivityResponse, StoreError> {
    let stored = store.read(|docs| docs.get_all::<McpCallData>()).await?;
    Ok(summarize_calls(&stored, query, now))
}
