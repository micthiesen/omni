//! `shell-run`: one record per `shell` call, and the `shell_usage` summary
//! that weighs the shell against direct tool calls.
//!
//! A record keeps the script (bounded), its outcome, the bytes it returned
//! and the tool data it read, so later analysis can tell whether scripts
//! filter tool results down or just pass them through. Records are kept for
//! the newest [`MAX_RUNS`] runs. Direct calls come from MCP activity, whose
//! rows record their result size.

use std::collections::{BTreeMap, HashMap, HashSet};

use omni_store::cbor::Extra;
use omni_store::entity::{Entity, EntityOps, EntityWrite, UpsertOpts};
use omni_store::{Store, StoreError};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::activity::McpCallData;

const LOG: &str = "MCP:Shell";
pub(crate) const MAX_RUNS: usize = 5_000;
const PRUNE_SLACK: u64 = 100;
/// UTF-16 units of a script kept in its record.
pub(crate) const SCRIPT_LIMIT: usize = 2_000;
const ERROR_LIMIT: usize = 1_000;
const FAILURE_SCRIPT_LIMIT: usize = 200;
const RECENT_FAILURES: usize = 5;
const BY_TOOL_LIMIT: usize = 25;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ShellOutcome {
    /// The script exited 0.
    Ok,
    /// The script exited nonzero.
    Failed,
    /// The timeout or another sandbox limit stopped it.
    Limited,
    /// The shell could not run the script (a parse error or an internal failure).
    Error,
    /// The request went away before the script finished.
    Interrupted,
    /// An outcome this build does not know (written by a newer one).
    #[serde(other)]
    Unknown,
}

/// One tool call a run made.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShellCall {
    pub tool: String,
    pub via: String,
    pub duration_ms: i64,
    pub bytes: i64,
    pub ok: bool,
    #[serde(flatten)]
    pub extra: Extra,
}

/// A run's calls and result bytes for one tool.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShellToolTotal {
    pub tool: String,
    pub calls: i64,
    pub bytes: i64,
    #[serde(flatten)]
    pub extra: Extra,
}

/// `shell-run`, keyed by `runId`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShellRunData {
    pub run_id: String,
    pub started_at: i64,
    pub duration_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// The script, cut to [`SCRIPT_LIMIT`] UTF-16 units.
    pub script: String,
    /// The script's full length in UTF-16 units.
    pub script_chars: i64,
    pub outcome: ShellOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Bytes the script printed, before the response cut them.
    pub stdout_bytes: i64,
    pub stderr_bytes: i64,
    /// Bytes of stdout and stderr in the response.
    pub returned_bytes: i64,
    pub truncated: bool,
    pub tool_calls: i64,
    /// Bytes of tool results the script read (commands and files), as the
    /// compact JSON a direct call returns.
    pub tool_bytes: i64,
    pub by_tool: Vec<ShellToolTotal>,
    /// The first calls, in order.
    pub calls: Vec<ShellCall>,
    #[serde(flatten)]
    pub extra: Extra,
}

impl Entity for ShellRunData {
    const NAME: &'static str = "shell-run";
    type Key = String;
    fn key(&self) -> String {
        self.run_id.clone()
    }
}

pub(crate) fn bound(text: &str, limit: usize) -> String {
    omni_mcp_kit::truncate_utf16(text, limit).0
}

pub(crate) fn bound_error(text: &str) -> String {
    bound(text, ERROR_LIMIT)
}

/// Stores a run and prunes the oldest beyond [`MAX_RUNS`]; failures are
/// logged and never change the run's result.
pub(crate) async fn record(store: &Store, run: ShellRunData) {
    let outcome = store
        .write(move |tx| {
            tx.upsert(&run, UpsertOpts::default())?;
            if tx.count::<ShellRunData>()? > MAX_RUNS as u64 + PRUNE_SLACK {
                let mut runs = tx.get_all::<ShellRunData>()?;
                runs.sort_by_key(|run| std::cmp::Reverse(run.started_at));
                for stale in runs.into_iter().skip(MAX_RUNS) {
                    tx.delete::<ShellRunData>(&stale.run_id)?;
                }
            }
            Ok::<_, StoreError>(())
        })
        .await;
    if let Err(error) = outcome {
        tracing::warn!(target: LOG, "Shell run record failed: {error}");
    }
}

/// The served tool list against the shell's own definition.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct SchemaSizes {
    pub served_tools: i64,
    pub tools_list_bytes: i64,
    pub shell_tool_bytes: i64,
    pub shell_commands: i64,
}

fn median(values: &mut [i64]) -> Option<i64> {
    percentile(values, 50)
}

/// The nearest-rank percentile.
fn percentile(values: &mut [i64], pct: usize) -> Option<i64> {
    if values.is_empty() {
        return None;
    }
    values.sort_unstable();
    let rank = (values.len() * pct).div_ceil(100).max(1);
    values.get(rank - 1).copied()
}

fn iso(ms: i64) -> String {
    jiff::Timestamp::from_millisecond(ms)
        .map(|t| t.to_string())
        .unwrap_or_default()
}

/// The `shell_usage` body for everything started at or after `since`.
pub(crate) fn summarize(
    runs: &[ShellRunData],
    calls: &[McpCallData],
    since: i64,
    schemas: SchemaSizes,
) -> Value {
    let mut runs: Vec<&ShellRunData> = runs.iter().filter(|r| r.started_at >= since).collect();
    runs.sort_by_key(|run| std::cmp::Reverse(run.started_at));
    let count = |outcome: ShellOutcome| runs.iter().filter(|r| r.outcome == outcome).count();
    let mut durations: Vec<i64> = runs.iter().map(|r| r.duration_ms).collect();
    let tool_bytes: i64 = runs.iter().map(|r| r.tool_bytes).sum();
    let returned_bytes: i64 = runs.iter().map(|r| r.returned_bytes).sum();
    let mut by_tool: BTreeMap<&str, (i64, i64)> = BTreeMap::new();
    for total in runs.iter().flat_map(|r| &r.by_tool) {
        let entry = by_tool.entry(&total.tool).or_default();
        entry.0 += total.calls;
        entry.1 += total.bytes;
    }
    let mut by_tool: Vec<(&str, (i64, i64))> = by_tool.into_iter().collect();
    by_tool.sort_by_key(|(_, (calls, bytes))| std::cmp::Reverse((*calls, *bytes)));
    let sessions: HashSet<&str> = runs.iter().filter_map(|r| r.session.as_deref()).collect();
    let failures: Vec<Value> = runs
        .iter()
        .filter(|r| r.outcome != ShellOutcome::Ok)
        .take(RECENT_FAILURES)
        .map(|r| {
            json!({
                "at": iso(r.started_at),
                "runId": r.run_id,
                "exitCode": r.exit_code,
                "error": r.error,
                "script": bound(&r.script, FAILURE_SCRIPT_LIMIT),
            })
        })
        .collect();
    let shell = json!({
        "runs": runs.len(),
        "sessions": sessions.len(),
        "succeeded": count(ShellOutcome::Ok),
        "failed": count(ShellOutcome::Failed) + count(ShellOutcome::Error),
        "interrupted": count(ShellOutcome::Interrupted),
        "limited": count(ShellOutcome::Limited),
        "truncated": runs.iter().filter(|r| r.truncated).count(),
        "medianDurationMs": median(&mut durations),
        "p90DurationMs": percentile(&mut durations, 90),
        "toolCalls": runs.iter().map(|r| r.tool_calls).sum::<i64>(),
        "toolBytes": tool_bytes,
        "returnedBytes": returned_bytes,
        "reduction": (tool_bytes > 0).then(|| {
            let ratio = 1.0 - returned_bytes as f64 / tool_bytes as f64;
            (ratio * 1000.0).round() / 1000.0
        }),
        "byTool": by_tool
            .into_iter()
            .take(BY_TOOL_LIMIT)
            .map(|(tool, (calls, bytes))| json!({"tool": tool, "calls": calls, "bytes": bytes}))
            .collect::<Vec<_>>(),
        "recentFailures": failures,
    });

    let window: Vec<&McpCallData> = calls.iter().filter(|c| c.started_at >= since).collect();
    let (shell_calls, direct): (Vec<&McpCallData>, Vec<&McpCallData>) = window
        .into_iter()
        .filter(|c| c.tool != super::defs::SHELL_USAGE.info().name)
        .partition(|c| c.tool == super::defs::SHELL.info().name);
    let mut sizes: Vec<i64> = direct.iter().filter_map(|c| c.result_bytes).collect();
    struct ToolAcc {
        read_only: bool,
        calls: i64,
        sizes: Vec<i64>,
    }
    let mut tools: HashMap<&str, ToolAcc> = HashMap::new();
    for call in &direct {
        let acc = tools.entry(&call.tool).or_insert_with(|| ToolAcc {
            read_only: call.read_only,
            calls: 0,
            sizes: Vec::new(),
        });
        acc.calls += 1;
        acc.sizes.extend(call.result_bytes);
    }
    let mut tools: Vec<(&str, ToolAcc)> = tools.into_iter().collect();
    tools.sort_by_key(|(tool, acc)| {
        (
            std::cmp::Reverse(acc.sizes.iter().sum::<i64>()),
            std::cmp::Reverse(acc.calls),
            *tool,
        )
    });
    let direct_json = json!({
        "calls": direct.len(),
        "measured": sizes.len(),
        "resultBytes": sizes.iter().sum::<i64>(),
        "medianResultBytes": median(&mut sizes),
        "shellCalls": shell_calls.len(),
        "shellResultBytes": shell_calls.iter().filter_map(|c| c.result_bytes).sum::<i64>(),
        "byTool": tools
            .into_iter()
            .take(BY_TOOL_LIMIT)
            .map(|(tool, mut acc)| {
                json!({
                    "tool": tool,
                    "readOnly": acc.read_only,
                    "calls": acc.calls,
                    "resultBytes": acc.sizes.iter().sum::<i64>(),
                    "medianResultBytes": median(&mut acc.sizes),
                    "maxResultBytes": acc.sizes.iter().max(),
                })
            })
            .collect::<Vec<_>>(),
    });
    json!({
        "since": iso(since),
        "shell": shell,
        "direct": direct_json,
        "schemas": {
            "servedTools": schemas.served_tools,
            "toolsListBytes": schemas.tools_list_bytes,
            "shellToolBytes": schemas.shell_tool_bytes,
            "shellCommands": schemas.shell_commands,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles_use_the_nearest_rank() {
        assert_eq!(percentile(&mut [], 50), None);
        assert_eq!(percentile(&mut [7], 90), Some(7));
        assert_eq!(percentile(&mut [4, 1, 3, 2], 50), Some(2));
        assert_eq!(
            percentile(&mut [1, 2, 3, 4, 5, 6, 7, 8, 9, 10], 90),
            Some(9)
        );
    }
}
