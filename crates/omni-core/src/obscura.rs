//! A headless Obscura browser driven over its stdio MCP server.
//!
//! The child runs `obscura mcp --stealth` with a cleared environment. Obscura
//! blocks loopback, private and link-local addresses by default; it is never
//! given `--allow-private-network`, so pages cannot reach the LAN. Requests
//! are strictly sequential (one caller owns the browser), so a reply is
//! simply the next line carrying the request's id.

use std::process::Stdio;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{
    AsyncBufReadExt as _, AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _, BufReader,
};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, thiserror::Error)]
pub enum ObscuraError {
    /// The browser could not be started.
    #[error("{0}")]
    Unavailable(String),
    /// The browser failed, hung or answered with an error.
    #[error("{0}")]
    Browser(String),
}

/// Per-caller bounds.
#[derive(Clone, Copy, Debug)]
pub struct ObscuraLimits {
    /// Bound on one JSON-RPC line from the browser; a longer reply fails the call.
    pub max_line_bytes: u64,
    /// Bound on one navigation.
    pub navigate_timeout: Duration,
}

/// One browser process with one page. Dropping it ends the process.
pub struct ObscuraBrowser {
    // Held so `kill_on_drop` ends the process with the browser.
    _child: Child,
    channel: Channel<ChildStdin, ChildStdout>,
}

/// The JSON-RPC exchange over the browser's pipes.
struct Channel<W, R> {
    stdin: W,
    stdout: BufReader<R>,
    next_id: i64,
    limits: ObscuraLimits,
}

#[derive(Deserialize)]
struct CallResult {
    #[serde(default)]
    content: Vec<Content>,
    #[serde(default, rename = "isError")]
    is_error: bool,
}

#[derive(Deserialize)]
struct Content {
    #[serde(default)]
    text: Option<String>,
}

impl ObscuraBrowser {
    /// Starts `program mcp --stealth` and completes the MCP handshake.
    pub async fn start(program: &str, limits: ObscuraLimits) -> Result<Self, ObscuraError> {
        let mut child = Command::new(program)
            .args(["mcp", "--stealth"])
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", std::env::temp_dir())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| ObscuraError::Unavailable(format!("could not start {program}: {e}")))?;
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            return Err(ObscuraError::Unavailable(
                "the browser's pipes are unavailable".into(),
            ));
        };
        let mut channel = Channel::new(stdin, stdout, limits);
        channel
            .request(
                "initialize",
                json!({
                    "protocolVersion": "2025-06-18",
                    "capabilities": {},
                    "clientInfo": {"name": "omni", "version": env!("CARGO_PKG_VERSION")},
                }),
                STARTUP_TIMEOUT,
            )
            .await?;
        channel
            .write(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
            .await?;
        Ok(Self {
            _child: child,
            channel,
        })
    }

    /// Loads `url`. Obscura answers only after the page's own scripts have
    /// run, whatever `waitUntil` says.
    pub async fn navigate(&mut self, url: &str) -> Result<(), ObscuraError> {
        self.channel.navigate(url).await
    }

    /// The expression's result as text (a returned string is unquoted).
    pub async fn evaluate(
        &mut self,
        expression: &str,
        timeout: Duration,
    ) -> Result<String, ObscuraError> {
        self.channel.evaluate(expression, timeout).await
    }
}

impl<W: AsyncWrite + Unpin + Send, R: AsyncRead + Unpin + Send> Channel<W, R> {
    fn new(stdin: W, stdout: R, limits: ObscuraLimits) -> Self {
        Self {
            stdin,
            stdout: BufReader::new(stdout),
            next_id: 1,
            limits,
        }
    }

    async fn write(&mut self, message: &Value) -> Result<(), ObscuraError> {
        let mut line = serde_json::to_vec(message)
            .map_err(|e| ObscuraError::Browser(format!("could not encode a request: {e}")))?;
        line.push(b'\n');
        self.stdin
            .write_all(&line)
            .await
            .map_err(|e| ObscuraError::Browser(format!("the browser stopped: {e}")))?;
        self.stdin
            .flush()
            .await
            .map_err(|e| ObscuraError::Browser(format!("the browser stopped: {e}")))
    }

    async fn request(
        &mut self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, ObscuraError> {
        let id = self.next_id;
        self.next_id += 1;
        // A tool call is named by its tool, so a timeout says which step hung.
        let label = params
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or(method)
            .to_owned();
        let max_line_bytes = self.limits.max_line_bytes;
        let exchange = async {
            self.write(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
                .await?;
            loop {
                let mut line = Vec::new();
                let read = (&mut self.stdout)
                    .take(max_line_bytes)
                    .read_until(b'\n', &mut line)
                    .await
                    .map_err(|e| ObscuraError::Browser(format!("the browser stopped: {e}")))?;
                if read == 0 {
                    return Err(ObscuraError::Browser("the browser exited".into()));
                }
                if !line.ends_with(b"\n") {
                    return Err(ObscuraError::Browser(
                        "the browser sent an oversized reply".into(),
                    ));
                }
                let Ok(message) = serde_json::from_slice::<Value>(&line) else {
                    continue;
                };
                if message.get("id").and_then(Value::as_i64) != Some(id) {
                    continue;
                }
                if let Some(error) = message.get("error") {
                    let text = error
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown");
                    return Err(ObscuraError::Browser(format!("{method} failed: {text}")));
                }
                return Ok(message.get("result").cloned().unwrap_or(Value::Null));
            }
        };
        tokio::time::timeout(timeout, exchange).await.map_err(|_| {
            ObscuraError::Browser(format!(
                "{label} got no answer within {} seconds",
                timeout.as_secs()
            ))
        })?
    }

    async fn call(
        &mut self,
        tool: &str,
        arguments: Value,
        timeout: Duration,
    ) -> Result<String, ObscuraError> {
        let result = self
            .request(
                "tools/call",
                json!({"name": tool, "arguments": arguments}),
                timeout,
            )
            .await?;
        let result: CallResult = serde_json::from_value(result).map_err(|e| {
            ObscuraError::Browser(format!("{tool} returned an invalid result: {e}"))
        })?;
        let text: String = result.content.into_iter().filter_map(|c| c.text).collect();
        if result.is_error {
            let first = text.lines().next().unwrap_or("unknown error");
            return Err(ObscuraError::Browser(format!("{tool}: {first}")));
        }
        Ok(text)
    }

    async fn navigate(&mut self, url: &str) -> Result<(), ObscuraError> {
        // Waiting for every image and ad frame (`load`) takes longer still.
        let arguments = json!({"url": url, "waitUntil": "domcontentloaded"});
        let timeout = self.limits.navigate_timeout;
        self.call("browser_navigate", arguments, timeout)
            .await
            .map(drop)
    }

    async fn evaluate(
        &mut self,
        expression: &str,
        timeout: Duration,
    ) -> Result<String, ObscuraError> {
        let millis = u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX);
        self.call(
            "browser_evaluate",
            json!({"expression": expression, "timeoutMs": millis}),
            timeout + Duration::from_secs(5),
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader, DuplexStream};

    use super::*;

    const LIMITS: ObscuraLimits = ObscuraLimits {
        max_line_bytes: 4 * 1024 * 1024,
        navigate_timeout: Duration::from_secs(60),
    };

    /// A browser that answers each request with `text` after `delay`, or never.
    fn browser_answering(
        delay: Option<Duration>,
        text: String,
        limits: ObscuraLimits,
    ) -> Channel<DuplexStream, DuplexStream> {
        let (stdin, requests) = tokio::io::duplex(64 * 1024);
        let (mut replies, stdout) = tokio::io::duplex(64 * 1024);
        tokio::spawn(async move {
            let mut lines = BufReader::new(requests).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let request: Value = serde_json::from_str(&line).unwrap();
                let Some(delay) = delay else {
                    std::future::pending::<()>().await;
                    return;
                };
                tokio::time::sleep(delay).await;
                let reply = json!({
                    "jsonrpc": "2.0",
                    "id": request["id"],
                    "result": {"content": [{"type": "text", "text": text}]},
                });
                let mut line = serde_json::to_vec(&reply).unwrap();
                line.push(b'\n');
                if replies.write_all(&line).await.is_err() {
                    return;
                }
            }
        });
        Channel::new(stdin, stdout, limits)
    }

    #[tokio::test(start_paused = true)]
    async fn navigation_waits_for_a_slow_page_within_its_bound() {
        let mut browser =
            browser_answering(Some(Duration::from_secs(35)), "Navigated".into(), LIMITS);
        let started = tokio::time::Instant::now();
        browser.navigate("https://example.com/").await.unwrap();
        assert_eq!(started.elapsed(), Duration::from_secs(35));
    }

    #[tokio::test(start_paused = true)]
    async fn silent_browser_fails_at_the_navigation_bound_naming_the_step() {
        let mut browser = browser_answering(None, String::new(), LIMITS);
        let started = tokio::time::Instant::now();
        let error = browser.navigate("https://example.com/").await.unwrap_err();
        assert_eq!(started.elapsed(), LIMITS.navigate_timeout);
        assert!(
            matches!(&error, ObscuraError::Browser(text)
                if text == "browser_navigate got no answer within 60 seconds"),
            "{error}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_reply_over_the_line_bound_fails_instead_of_being_buffered() {
        let limits = ObscuraLimits {
            max_line_bytes: 1024,
            ..LIMITS
        };
        let mut browser = browser_answering(Some(Duration::ZERO), "x".repeat(4096), limits);
        let error = browser
            .evaluate("document.title", Duration::from_secs(5))
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ObscuraError::Browser(text)
                if text == "the browser sent an oversized reply"),
            "{error}"
        );
    }
}
