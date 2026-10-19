//! Stdio sessions with a bundled MCP server process.
//!
//! The child gets a cleared environment: `PATH`, `HOME`, `NODE_ENV` and the
//! integration's configured variables only, so Omni's own secrets never
//! reach it. Messages are newline-delimited JSON-RPC; stderr is kept as a
//! short tail for diagnostics. The process is killed when the session
//! closes or Omni stops, and its exit fails every pending request.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use futures::future::BoxFuture;
use serde_json::{Map, Value, json};
use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use super::{
    CONNECT_TIMEOUT, CancelOnDrop, ClientError, Incoming, MAX_MESSAGE_BYTES, NoticeSender,
    RequestOptions, RpcError, RpcResult, Session, UpstreamNotice, answer_server_request,
    cancel_reason, cancelled_params, classify, initialize_params, negotiated_version, notice_for,
};

/// How far a request's line got into the server's stdin.
const UNWRITTEN: u8 = 0;
const WRITING: u8 = 1;
const WRITTEN: u8 = 2;

const LOG: &str = "MCP:Gateway";
const STDERR_TAIL: usize = 2_048;

/// What to run.
#[derive(Clone, PartialEq, Eq)]
pub struct StdioCommand {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

impl std::fmt::Debug for StdioCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StdioCommand")
            .field("program", &self.program)
            .field("args", &self.args)
            .field("env", &self.env.iter().map(|(k, _)| k).collect::<Vec<_>>())
            .finish()
    }
}

type Pending = HashMap<i64, oneshot::Sender<Result<Value, RpcError>>>;

struct Inner {
    stdin: tokio::sync::Mutex<ChildStdin>,
    pending: Mutex<Pending>,
    next_id: AtomicI64,
    alive: AtomicBool,
    stderr: Mutex<VecDeque<u8>>,
    /// Values never to repeat in diagnostics (the configured variables).
    secrets: Vec<String>,
    notices: NoticeSender,
    stop: CancellationToken,
    tracker: TaskTracker,
}

/// One running server process; cheap to clone.
#[derive(Clone)]
pub struct StdioSession {
    inner: Arc<Inner>,
    initialized: Arc<Value>,
}

/// Stops a server whose connect did not finish (failed, or abandoned by a
/// dropped caller), so no process outlives it.
struct StopUnlessConnected(Option<CancellationToken>);

impl Drop for StopUnlessConnected {
    fn drop(&mut self) {
        if let Some(stop) = self.0.take() {
            stop.cancel();
        }
    }
}

impl StdioSession {
    /// Starts the process and runs `initialize`.
    pub async fn connect(
        command: &StdioCommand,
        notices: NoticeSender,
        tracker: TaskTracker,
        stop: CancellationToken,
    ) -> Result<Self, ClientError> {
        let mut process = Command::new(&command.program);
        process
            .args(&command.args)
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", std::env::temp_dir())
            .env("NODE_ENV", "production")
            .envs(command.env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut connecting = StopUnlessConnected(Some(stop.clone()));
        let mut child = process.spawn().map_err(|e| {
            ClientError::Process(format!(
                "could not start {}: {e}",
                command.program.display()
            ))
        })?;
        let (Some(stdin), Some(stdout), Some(stderr)) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            return Err(ClientError::Process(
                "the server's pipes are unavailable".into(),
            ));
        };
        let inner = Arc::new(Inner {
            stdin: tokio::sync::Mutex::new(stdin),
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicI64::new(1),
            alive: AtomicBool::new(true),
            stderr: Mutex::new(VecDeque::new()),
            secrets: command
                .env
                .iter()
                .map(|(_, v)| v.clone())
                .filter(|v| v.len() >= 4)
                .collect(),
            notices,
            stop,
            tracker,
        });
        let reader = inner.clone();
        omni_core::spawn::spawn_tracked(&inner.tracker, "mcp-gateway-stdio-out", async move {
            reader.read_stdout(stdout).await;
        });
        let tail = inner.clone();
        omni_core::spawn::spawn_tracked(&inner.tracker, "mcp-gateway-stdio-err", async move {
            tail.read_stderr(stderr).await;
        });
        let waiter = inner.clone();
        omni_core::spawn::spawn_tracked(&inner.tracker, "mcp-gateway-stdio-wait", async move {
            waiter.supervise(child).await;
        });
        let options = RequestOptions::new(CONNECT_TIMEOUT);
        let result = match inner
            .call("initialize", initialize_params(), &options)
            .await
        {
            Ok(Ok(result)) => result,
            Ok(Err(error)) => {
                return Err(ClientError::Protocol(format!(
                    "initialize failed: {} ({})",
                    error.message, error.code
                )));
            }
            Err(error) => return Err(error),
        };
        negotiated_version(&result)?;
        inner
            .write(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
            .await?;
        connecting.0 = None;
        Ok(Self {
            inner,
            initialized: Arc::new(result),
        })
    }
}

/// Reads one `\n`-terminated line of at most `limit` bytes.
async fn next_line(
    reader: &mut BufReader<ChildStdout>,
    line: &mut Vec<u8>,
    limit: usize,
) -> std::io::Result<bool> {
    line.clear();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Ok(!line.is_empty());
        }
        if let Some(end) = available.iter().position(|b| *b == b'\n') {
            line.extend_from_slice(&available[..end]);
            reader.consume(end + 1);
            return Ok(true);
        }
        let taken = available.len();
        line.extend_from_slice(available);
        reader.consume(taken);
        if line.len() > limit {
            return Err(std::io::Error::other("message exceeds the size limit"));
        }
    }
}

impl Inner {
    async fn write(&self, message: &Value) -> Result<(), ClientError> {
        self.write_tracked(message, &AtomicU8::new(UNWRITTEN)).await
    }

    /// Writes one line, moving `progress` to [`WRITING`] once it holds the
    /// stdin lock and to [`WRITTEN`] after the flush.
    async fn write_tracked(&self, message: &Value, progress: &AtomicU8) -> Result<(), ClientError> {
        let mut line = serde_json::to_vec(message).unwrap_or_default();
        line.push(b'\n');
        let mut stdin = self.stdin.lock().await;
        progress.store(WRITING, Ordering::SeqCst);
        let result = stdin
            .write_all(&line)
            .await
            .and(stdin.flush().await)
            .map_err(|e| ClientError::Process(format!("the server stopped reading: {e}")));
        progress.store(WRITTEN, Ordering::SeqCst);
        result
    }

    /// One request: the write and the answer are both bounded by the
    /// timeout and cancellation. A request that does not finish sends
    /// `notifications/cancelled`, also when the caller is dropped. Only a
    /// write cut off halfway stops the server (it would corrupt the stream);
    /// a request abandoned before its write began never reached it.
    async fn call(
        self: &Arc<Self>,
        method: &str,
        params: Map<String, Value>,
        options: &RequestOptions,
    ) -> RpcResult {
        if !self.alive.load(Ordering::Relaxed) {
            return Err(ClientError::Process(
                self.exit_message("the server is not running"),
            ));
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = oneshot::channel();
        self.pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(id, sender);
        let progress = Arc::new(AtomicU8::new(UNWRITTEN));
        let me = self.clone();
        let seen = progress.clone();
        let mut guard = CancelOnDrop::new(move |reason: &'static str| {
            me.forget(id);
            match seen.load(Ordering::SeqCst) {
                UNWRITTEN => return,
                WRITING => {
                    me.stop.cancel();
                    return;
                }
                _ => {}
            }
            let tracker = me.tracker.clone();
            omni_core::spawn::spawn_tracked(&tracker, "mcp-gateway-stdio-cancel", async move {
                let note = json!({
                    "jsonrpc": "2.0",
                    "method": "notifications/cancelled",
                    "params": cancelled_params(id, reason),
                });
                if let Err(error) = me.write(&note).await {
                    tracing::debug!(target: LOG, %error, "Stdio cancellation not sent");
                }
            });
        });
        let message = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let exchange = async {
            self.write_tracked(&message, &progress).await?;
            match receiver.await {
                Ok(outcome) => Ok(outcome),
                Err(_) => Err(ClientError::Process(self.exit_message("the server exited"))),
            }
        };
        let outcome = tokio::select! {
            () = options.cancel.cancelled() => Err(ClientError::Cancelled),
            () = self.stop.cancelled() => Err(ClientError::Process(self.exit_message("the server stopped"))),
            answer = tokio::time::timeout(options.timeout, exchange) => answer.unwrap_or(Err(ClientError::Timeout)),
        };
        match &outcome {
            Err(ClientError::Cancelled | ClientError::Timeout) => {
                guard.fire(cancel_reason(options));
            }
            Err(_) => {
                self.forget(id);
                guard.disarm();
            }
            Ok(_) => guard.disarm(),
        }
        outcome
    }

    fn forget(&self, id: i64) {
        self.pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&id);
    }

    /// `prefix`, then the redacted stderr tail.
    fn exit_message(&self, prefix: &str) -> String {
        let tail: Vec<u8> = self
            .stderr
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .copied()
            .collect();
        let mut text = String::from_utf8_lossy(&tail).trim().to_owned();
        for secret in &self.secrets {
            text = text.replace(secret.as_str(), "<redacted>");
        }
        let last: String = text
            .lines()
            .rev()
            .take(3)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join(" / ");
        if last.is_empty() {
            prefix.to_owned()
        } else {
            format!("{prefix}: {}", last.chars().take(500).collect::<String>())
        }
    }

    async fn read_stdout(self: Arc<Self>, stdout: ChildStdout) {
        let mut reader = BufReader::new(stdout);
        let mut line = Vec::new();
        loop {
            match next_line(&mut reader, &mut line, MAX_MESSAGE_BYTES).await {
                Ok(true) => {}
                Ok(false) => return,
                Err(error) => {
                    tracing::warn!(target: LOG, %error, "Stdio server output unreadable");
                    self.stop.cancel();
                    return;
                }
            }
            let Ok(message) = serde_json::from_slice::<Value>(&line) else {
                continue;
            };
            match classify(&message) {
                Incoming::Response { id, outcome } => {
                    let sender = id.as_i64().and_then(|id| {
                        self.pending
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .remove(&id)
                    });
                    if let Some(sender) = sender {
                        let _ = sender.send(outcome);
                    }
                }
                Incoming::Request { id, method } => {
                    if let Err(error) = self.write(&answer_server_request(id, &method)).await {
                        tracing::debug!(target: LOG, %error, "Stdio server request not answered");
                    }
                }
                Incoming::Notification { method } => {
                    if let Some(notice) = notice_for(&method) {
                        let _ = self.notices.send(notice);
                    }
                }
                Incoming::Invalid => {}
            }
        }
    }

    async fn read_stderr(self: Arc<Self>, mut stderr: tokio::process::ChildStderr) {
        let mut buffer = [0u8; 1024];
        while let Ok(read) = stderr.read(&mut buffer).await {
            if read == 0 {
                return;
            }
            let mut tail = self.stderr.lock().unwrap_or_else(PoisonError::into_inner);
            tail.extend(&buffer[..read]);
            while tail.len() > STDERR_TAIL {
                tail.pop_front();
            }
        }
    }

    /// Waits for the process to exit, or kills it when the session closes.
    async fn supervise(self: Arc<Self>, mut child: Child) {
        let status = tokio::select! {
            status = child.wait() => status.ok(),
            () = self.stop.cancelled() => {
                if let Err(error) = child.start_kill() {
                    tracing::debug!(target: LOG, %error, "Stdio server kill failed");
                }
                child.wait().await.ok()
            }
        };
        self.alive.store(false, Ordering::Relaxed);
        self.stop.cancel();
        self.pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
        if let Some(status) = status
            && !status.success()
        {
            tracing::warn!(target: LOG, "{}", self.exit_message(&format!("Stdio MCP server exited ({status})")));
        }
        let _ = self.notices.send(UpstreamNotice::Closed);
    }
}

impl Session for StdioSession {
    fn initialized(&self) -> &Value {
        &self.initialized
    }

    fn request<'a>(
        &'a self,
        method: &'a str,
        params: Map<String, Value>,
        options: RequestOptions,
    ) -> BoxFuture<'a, RpcResult> {
        Box::pin(async move { self.inner.call(method, params, &options).await })
    }

    fn notify<'a>(&'a self, method: &'a str, params: Map<String, Value>) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let message = json!({"jsonrpc": "2.0", "method": method, "params": params});
            if let Err(error) = self.inner.write(&message).await {
                tracing::debug!(target: LOG, %error, method, "Stdio notification failed");
            }
        })
    }

    fn alive(&self) -> bool {
        self.inner.alive.load(Ordering::Relaxed)
    }

    fn close(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            self.inner.stop.cancel();
        })
    }
}
