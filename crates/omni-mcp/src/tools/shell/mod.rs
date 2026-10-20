//! `shell` and `shell_usage`: a sandboxed bash over Omni's read-only tools
//! (`docs/shell.md`).
//!
//! The shell is bashkit, an in-process bash interpreter with a virtual
//! filesystem: no host process, file or network is reachable. Every
//! read-only tool is a command and every argument-free one is also a file
//! under `/omni` ([`fs`]); both run the tool's own handler ([`run`]). Stylist
//! tools stay out, since their activity records keep only identifiers. Each
//! call gets a fresh shell unless it names a session, which keeps `/tmp`,
//! the working directory and variables for [`SESSION_IDLE_MS`]. Every run
//! is recorded for `shell_usage` ([`usage`]).

pub mod defs;

mod commands;
mod fs;
mod run;
mod usage;

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Duration;

use bashkit::{Bash, ExecutionExtensions, ExecutionLimits, FsLimits};
use futures::future::BoxFuture;
use omni_core::clock::SharedClock;
use omni_mcp_kit::{
    McpTool, SchemaValidator, ToolContext, ToolDefinition, ToolError, ToolHandler, ToolMetaError,
    ToolOutput, raw_tool, typed_tool,
};
use omni_store::Store;
use omni_store::entity::EntityOps;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tokio::sync::{Mutex, Semaphore};
use tokio_util::task::TaskTracker;

use self::commands::{Command, ToolCommand, ToolsCommand};
use self::fs::{MOUNT, OmniFs};
use self::run::{Calls, RECORDED_CALLS, Run};
use self::usage::{SCRIPT_LIMIT, SchemaSizes};
pub use self::usage::{ShellCall, ShellOutcome, ShellRunData, ShellToolTotal};
use super::conform;

const LOG: &str = "MCP:Shell";

/// How long one script may run.
const TIMEOUT: Duration = Duration::from_secs(60);
/// UTF-16 units of stdout and of stderr a response carries.
const STDOUT_LIMIT: usize = 60_000;
const STDERR_LIMIT: usize = 8_000;
/// Sessions kept at once; the least recently used goes first.
const MAX_SESSIONS: usize = 16;
pub const SESSION_IDLE_MS: i64 = 30 * 60 * 1000;

/// Scripts running at once across every caller; more wait.
const CONCURRENT_RUNS: usize = 4;
/// The in-memory filesystem of one shell (`/tmp`).
const FS_TOTAL_BYTES: u64 = 16 * 1024 * 1024;
const FS_FILE_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Clone)]
pub struct ShellDeps {
    pub store: Store,
    pub clock: SharedClock,
    /// Records the runs whose request went away.
    pub tracker: TaskTracker,
}

struct Session {
    bash: Bash,
    last_used: i64,
    /// Set while a script runs; still set when the next call arrives means
    /// the last run was dropped mid-script, so the shell is replaced.
    running: bool,
}

struct Inner {
    deps: ShellDeps,
    commands: Arc<Vec<Arc<Command>>>,
    files: Arc<BTreeMap<String, Arc<Command>>>,
    readme: String,
    sessions: std::sync::Mutex<HashMap<String, Arc<Mutex<Session>>>>,
    runs: Semaphore,
    schemas: SchemaSizes,
}

/// Whether a served tool becomes a shell command.
fn is_command(tool: &McpTool) -> bool {
    let name = tool.meta.name.as_str();
    tool.meta.annotations.read_only_hint
        && !name.starts_with("stylist_")
        && name != defs::SHELL.info().name
        && name != defs::SHELL_USAGE.info().name
}

/// `shell` and `shell_usage` over `tools`, every other served tool.
pub fn shell_tools(tools: &[McpTool], deps: ShellDeps) -> Result<Vec<McpTool>, ToolMetaError> {
    let commands: Vec<Arc<Command>> = tools
        .iter()
        .filter(|tool| is_command(tool))
        .map(|tool| Arc::new(Command::new(tool.clone())))
        .collect();
    let files: BTreeMap<String, Arc<Command>> = commands
        .iter()
        .filter(|command| command.runs_bare())
        .map(|command| (command.name().to_owned(), command.clone()))
        .collect();
    let listed = |meta: &omni_mcp_kit::ToolMeta| -> i64 {
        let text = omni_core::js::json_stringify(&Value::Object(meta.listed()));
        i64::try_from(text.len()).unwrap_or(i64::MAX)
    };
    let shell_meta = defs::SHELL.meta()?;
    let usage_meta = defs::SHELL_USAGE.meta()?;
    let schemas = SchemaSizes {
        served_tools: i64::try_from(tools.len() + 2).unwrap_or(i64::MAX),
        tools_list_bytes: tools.iter().map(|t| listed(t.meta)).sum::<i64>()
            + listed(shell_meta)
            + listed(usage_meta),
        shell_tool_bytes: listed(shell_meta),
        shell_commands: i64::try_from(commands.len()).unwrap_or(i64::MAX),
    };
    let shell = Shell(Arc::new(Inner {
        readme: readme(&commands, &files),
        deps,
        commands: Arc::new(commands),
        files: Arc::new(files),
        sessions: std::sync::Mutex::new(HashMap::new()),
        runs: Semaphore::new(CONCURRENT_RUNS),
        schemas,
    }));
    let usage = shell.clone();
    let validator = SchemaValidator::new(&shell_meta.name, &shell_meta.input_schema)?;
    Ok(vec![
        raw_tool(&defs::SHELL, Arc::new(ShellHandler { shell, validator }))?,
        typed_tool(
            &defs::SHELL_USAGE,
            move |input: ShellUsageInput, _: ToolContext| {
                let shell = usage.clone();
                async move { shell.usage(input.days.unwrap_or(14)).await }
            },
        )?,
    ])
}

/// `shell`'s handler: its result's text content is the script's output
/// itself, not the structured result as JSON.
struct ShellHandler {
    shell: Shell,
    validator: SchemaValidator,
}

impl ToolHandler for ShellHandler {
    fn call<'a>(
        &'a self,
        input: Value,
        cx: ToolContext,
    ) -> BoxFuture<'a, Result<ToolOutput, ToolError>> {
        Box::pin(async move {
            self.validator.check(&input).map_err(ToolError::input)?;
            let input: ShellInput =
                serde_json::from_value(input).map_err(|e| ToolError::input(e.to_string()))?;
            let (structured, text) = Box::pin(self.shell.clone().run(input, cx)).await?;
            Ok(ToolOutput::Custom {
                structured,
                content: vec![text_content(text)],
            })
        })
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ShellInput {
    script: String,
    session: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ShellUsageInput {
    days: Option<u32>,
}

#[derive(Clone)]
struct Shell(Arc<Inner>);

/// What one exec produced, before bounding.
struct Executed {
    exit_code: Option<i64>,
    stdout: String,
    stderr: String,
    outcome: ShellOutcome,
    error: Option<String>,
    cut_by_shell: bool,
    /// The shell may be left mid-command; a session gets a fresh one.
    replace_shell: bool,
}

/// A run's `shell-run` row: written by [`RunRecord::finish`], or as
/// `interrupted` when the request goes away before the run finishes.
struct RunRecord {
    store: Store,
    clock: SharedClock,
    tracker: TaskTracker,
    run: Arc<Run>,
    row: Option<ShellRunData>,
}

impl RunRecord {
    fn new(
        deps: &ShellDeps,
        run: Arc<Run>,
        run_id: &str,
        started: i64,
        input: &ShellInput,
    ) -> Self {
        let chars = omni_core::js::utf16_len(&input.script);
        Self {
            store: deps.store.clone(),
            clock: deps.clock.clone(),
            tracker: deps.tracker.clone(),
            run,
            row: Some(ShellRunData {
                run_id: run_id.to_owned(),
                started_at: started,
                duration_ms: 0,
                session: input.session.clone(),
                script: usage::bound(&input.script, SCRIPT_LIMIT),
                script_chars: i64::try_from(chars).unwrap_or(i64::MAX),
                outcome: ShellOutcome::Interrupted,
                exit_code: None,
                error: None,
                stdout_bytes: 0,
                stderr_bytes: 0,
                returned_bytes: 0,
                truncated: false,
                tool_calls: 0,
                tool_bytes: 0,
                by_tool: Vec::new(),
                calls: Vec::new(),
                extra: Default::default(),
            }),
        }
    }

    /// The row with its duration and tool calls filled in.
    fn complete(&mut self, finished: i64) -> Option<ShellRunData> {
        let mut row = self.row.take()?;
        row.duration_ms = finished - row.started_at;
        add_calls(&mut row, self.run.take_calls());
        Some(row)
    }

    async fn finish(mut self, finished: i64) {
        if let Some(row) = self.complete(finished) {
            usage::record(&self.store, row).await;
        }
    }
}

impl Drop for RunRecord {
    fn drop(&mut self) {
        let finished = self.clock.now_ms();
        if let Some(mut row) = self.complete(finished) {
            row.error = Some("The MCP request was cancelled".to_owned());
            let store = self.store.clone();
            omni_core::spawn::spawn_tracked(&self.tracker, "shell-run-interrupted", async move {
                usage::record(&store, row).await;
            });
        }
    }
}

fn add_calls(row: &mut ShellRunData, calls: Calls) {
    row.tool_calls = i64::try_from(calls.count).unwrap_or(i64::MAX);
    row.tool_bytes = calls.bytes;
    row.by_tool = calls
        .by_tool
        .into_iter()
        .map(|(tool, (calls, bytes))| ShellToolTotal {
            tool,
            calls,
            bytes,
            extra: Default::default(),
        })
        .collect();
    row.calls = calls
        .records
        .into_iter()
        .take(RECORDED_CALLS)
        .map(|call| ShellCall {
            tool: call.tool,
            via: call.via.as_str().to_owned(),
            duration_ms: call.duration_ms,
            bytes: call.bytes,
            ok: call.ok,
            extra: Default::default(),
        })
        .collect();
}

impl Shell {
    fn new_bash(&self) -> Bash {
        let inner = &self.0;
        // `date` follows the app clock: the offset from the host clock is
        // fixed when the shell starts. The host clock is read only for that
        // offset, so tests on a paused clock still see the app's time.
        let now_ms = inner.deps.clock.now_ms();
        let host_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(now_ms, |d| i64::try_from(d.as_millis()).unwrap_or(now_ms));
        let mut builder = Bash::builder()
            .username("omni")
            .hostname("omni")
            .env("HOME", "/tmp")
            .env("TZ", "UTC")
            .cwd("/tmp")
            .epoch_offset((now_ms - host_ms) / 1000)
            .limits(ExecutionLimits::new().timeout(TIMEOUT))
            .filesystem_limits(
                FsLimits::new()
                    .max_total_bytes(FS_TOTAL_BYTES)
                    .max_file_size(FS_FILE_BYTES),
            )
            .mount_readonly_text("/README.md", inner.readme.clone())
            .builtin("tools", Box::new(ToolsCommand(inner.commands.clone())));
        for command in inner.commands.iter() {
            builder = builder.builtin(command.name(), Box::new(ToolCommand(command.clone())));
        }
        builder.build()
    }

    /// The session's shell, created or reclaimed; idle and excess sessions
    /// are dropped first.
    fn session(&self, name: &str, now: i64) -> Arc<Mutex<Session>> {
        let Ok(mut sessions) = self.0.sessions.lock() else {
            return Arc::new(Mutex::new(Session {
                bash: self.new_bash(),
                last_used: now,
                running: false,
            }));
        };
        sessions.retain(|_, session| {
            session
                .try_lock()
                .map_or(true, |s| now - s.last_used < SESSION_IDLE_MS)
        });
        if let Some(session) = sessions.get(name) {
            return session.clone();
        }
        // Evict the least recently used idle session; running ones stay, so
        // the map can briefly exceed the cap when every session is busy.
        while sessions.len() >= MAX_SESSIONS {
            let oldest = sessions
                .iter()
                .filter_map(|(name, s)| s.try_lock().ok().map(|s| (name, s.last_used)))
                .min_by_key(|(_, last_used)| *last_used)
                .map(|(name, _)| name.clone());
            match oldest {
                Some(oldest) => sessions.remove(&oldest),
                None => break,
            };
        }
        let session = Arc::new(Mutex::new(Session {
            bash: self.new_bash(),
            last_used: now,
            running: false,
        }));
        sessions.insert(name.to_owned(), session.clone());
        session
    }

    /// The structured result and the text content.
    async fn run(
        self,
        input: ShellInput,
        cx: ToolContext,
    ) -> Result<(Map<String, Value>, String), ToolError> {
        let inner = &self.0;
        let clock = inner.deps.clock.clone();
        let started = clock.now_ms();
        let run_id = omni_core::ids::uuid_v4();
        let run = Arc::new(Run::new(clock.clone(), cx.cancel.child_token()));
        let mut record = RunRecord::new(&inner.deps, run.clone(), &run_id, started, &input);
        let _permit = inner
            .runs
            .acquire()
            .await
            .map_err(|_| ToolError::execute("the shell is shutting down"))?;
        let executed = match &input.session {
            Some(name) => {
                let session = self.session(name, started);
                let mut session = session.lock().await;
                if session.running {
                    session.bash = self.new_bash();
                }
                session.running = true;
                let executed = self.exec(&mut session.bash, &input.script, &run).await;
                if executed.replace_shell {
                    session.bash = self.new_bash();
                }
                session.running = false;
                session.last_used = clock.now_ms();
                executed
            }
            None => {
                let mut bash = self.new_bash();
                self.exec(&mut bash, &input.script, &run).await
            }
        };
        let finished = clock.now_ms();
        let (stdout, stdout_cut) = cut(&executed.stdout, STDOUT_LIMIT);
        let (mut stderr, stderr_cut) = cut(&executed.stderr, STDERR_LIMIT);
        let truncated = stdout_cut || stderr_cut || executed.cut_by_shell;
        if stdout_cut {
            let _ = write!(
                stderr,
                "\n[shell: stdout was cut to {STDOUT_LIMIT} of {} characters; filter it with jq, grep or head, or write it to /tmp in a session]",
                omni_core::js::utf16_len(&executed.stdout)
            );
        }
        let byte_len = |n: usize| i64::try_from(n).unwrap_or(i64::MAX);
        if let Some(row) = record.row.as_mut() {
            row.outcome = executed.outcome;
            row.exit_code = executed.exit_code;
            row.error = executed.error.as_deref().map(usage::bound_error);
            row.stdout_bytes = byte_len(executed.stdout.len());
            row.stderr_bytes = byte_len(executed.stderr.len());
            row.returned_bytes = byte_len(stdout.len() + stderr.len());
            row.truncated = truncated;
        }
        record.finish(finished).await;
        let exit_code = executed.exit_code.unwrap_or(1);
        let text = reply_text(&stdout, &stderr, exit_code);
        let structured = json!({
            "exitCode": exit_code,
            "stdout": stdout,
            "stderr": stderr,
            "truncated": truncated,
            "durationMs": finished - started,
            "runId": run_id,
        });
        let Value::Object(structured) = conform(&defs::SHELL, structured)? else {
            return Err(ToolError::output("shell output must be an object"));
        };
        Ok((structured, text))
    }

    /// Runs `script` with `/omni` mounted for this run only.
    async fn exec(&self, bash: &mut Bash, script: &str, run: &Arc<Run>) -> Executed {
        let inner = &self.0;
        let omni = OmniFs::new(inner.files.clone(), run.clone(), inner.deps.clock.now_ms());
        if let Err(error) = bash.mount(MOUNT, Arc::new(omni)) {
            return Executed::failed(format!("mounting {MOUNT} failed: {error}"));
        }
        let extensions = ExecutionExtensions::new().with(run.clone());
        // A margin past bashkit's own timeout, for a builtin that ignores it.
        let outcome = tokio::time::timeout(
            TIMEOUT + Duration::from_secs(5),
            Box::pin(bash.exec_with_extensions(script, extensions)),
        )
        .await;
        if let Err(error) = bash.unmount(MOUNT) {
            tracing::warn!(target: LOG, "Unmounting {MOUNT} failed: {error}");
        }
        match outcome {
            // The dropped exec may have left the shell mid-command.
            Err(_) => Executed {
                replace_shell: true,
                ..Executed::limited(format!(
                    "the script ran past the {}-second limit",
                    TIMEOUT.as_secs()
                ))
            },
            Ok(Ok(result)) => Executed {
                exit_code: Some(i64::from(result.exit_code)),
                stdout: result.stdout.text_lossy().into_owned(),
                stderr: result.stderr.text_lossy().into_owned(),
                outcome: if result.exit_code == 0 {
                    ShellOutcome::Ok
                } else {
                    ShellOutcome::Failed
                },
                error: None,
                cut_by_shell: result.stdout_truncated || result.stderr_truncated,
                replace_shell: false,
            },
            Ok(Err(bashkit::Error::ResourceLimit(limit))) => Executed::limited(limit.to_string()),
            Ok(Err(bashkit::Error::Cancelled)) => {
                Executed::limited("the script was cancelled".into())
            }
            Ok(Err(error)) => Executed::failed(error.to_string()),
        }
    }

    async fn usage(&self, days: u32) -> Result<Value, ToolError> {
        let inner = &self.0;
        let since = inner.deps.clock.now_ms() - i64::from(days) * 24 * 60 * 60 * 1000;
        let (runs, calls) = inner
            .deps
            .store
            .read(|docs| {
                Ok::<_, omni_store::StoreError>((
                    docs.get_all::<ShellRunData>()?,
                    docs.get_all::<crate::activity::McpCallData>()?,
                ))
            })
            .await
            .map_err(|e| ToolError::execute_from(&e))?;
        conform(
            &defs::SHELL_USAGE,
            usage::summarize(&runs, &calls, since, inner.schemas),
        )
    }
}

impl Executed {
    fn limited(message: String) -> Self {
        Self {
            exit_code: Some(124),
            stdout: String::new(),
            stderr: format!("shell: {message}\n"),
            outcome: ShellOutcome::Limited,
            error: Some(message),
            cut_by_shell: false,
            replace_shell: false,
        }
    }

    fn failed(message: String) -> Self {
        Self {
            exit_code: Some(2),
            stdout: String::new(),
            stderr: format!("shell: {message}\n"),
            outcome: ShellOutcome::Error,
            error: Some(message),
            cut_by_shell: false,
            replace_shell: false,
        }
    }
}

fn cut(text: &str, limit: usize) -> (String, bool) {
    omni_mcp_kit::truncate_utf16(text, limit)
}

/// The text content a client shows its model: stdout, then stderr and a
/// nonzero exit status when there are any.
fn reply_text(stdout: &str, stderr: &str, exit_code: i64) -> String {
    let mut text = stdout.to_owned();
    if !stderr.is_empty() {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        let _ = write!(text, "[stderr]\n{stderr}");
    }
    if exit_code != 0 {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        let _ = write!(text, "[exit {exit_code}]");
    }
    if text.is_empty() {
        text.push_str("(no output)");
    }
    text
}

/// `/README.md`: the layout, the commands and the files.
fn readme(commands: &[Arc<Command>], files: &BTreeMap<String, Arc<Command>>) -> String {
    let mut text = String::from(
        "# Omni shell\n\
         \n\
         A sandboxed bash over Omni's data. Nothing here changes Omni or sends anything;\n\
         only what the script prints is returned.\n\
         \n\
         /omni/<tool>.json  results of the read-only tools that need no arguments, with\n\
         \x20                  their default arguments, read on first use in each call\n\
         /tmp               scratch space (the working directory; kept within a session)\n\
         /README.md         this file\n\
         \n\
         Every read-only Omni tool is a command that prints its result as indented JSON.\n\
         `tools` lists them; `<tool> --help` shows a tool's flags. Flags are the tool's\n\
         input fields in kebab-case or camelCase; array flags repeat; a bare boolean flag\n\
         means true; one JSON object argument is passed through as the whole input.\n\
         \n\
         \x20 email_search --from amazon --since 2026-10-01 --limit 10\n\
         \x20 email_get --email-id <id> --body-chars 2000\n\
         \x20 calendar_events_list --from 2026-10-20 --to 2026-10-27 | jq '.events[] | .title'\n\
         \x20 jq -r '.deliveries[] | [.trackingNumber, .statusLabel] | @tsv' /omni/parcels_list.json\n\
         \n\
         Reading a file runs its tool, so read the files you need rather than all of /omni.\n\
         Filter with jq, grep, sed, awk, sort, uniq, head and wc so the output stays small.\n\
         Output over 60,000 characters is cut. A script may make 200 tool calls and run\n\
         for 60 seconds. `date` reports UTC; tool results carry their own times.\n\
         \n\
         ## Files\n\n",
    );
    for (name, command) in files {
        let _ = writeln!(text, "/omni/{name}.json  {}", command.tool.meta.title);
    }
    text.push_str("\n## Commands\n\n");
    for command in commands {
        let _ = writeln!(text, "{}  {}", command.name(), command.tool.meta.title);
    }
    text
}

fn text_content(text: String) -> omni_mcp_kit::Content {
    let mut block = Map::new();
    block.insert("type".into(), Value::String("text".into()));
    block.insert("text".into(), Value::String(text));
    block
}
