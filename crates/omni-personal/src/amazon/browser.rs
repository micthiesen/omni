//! A headless Obscura browser driven over its stdio MCP server.
//!
//! The child runs `obscura mcp --stealth` with a cleared environment. Obscura
//! blocks loopback, private and link-local addresses by default; it is never
//! given `--allow-private-network`, so pages cannot reach the LAN. Requests
//! are strictly sequential (the caller holds the session lock), so a reply is
//! simply the next line carrying the request's id.

use std::process::Stdio;
use std::time::Duration;

use futures::future::BoxFuture;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{
    AsyncBufReadExt as _, AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _, BufReader,
};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

use super::AmazonError;

/// Bound on one JSON-RPC line from the browser.
const MAX_LINE_BYTES: u64 = 4 * 1024 * 1024;
const STARTUP_TIMEOUT: Duration = Duration::from_secs(15);
/// Obscura answers a navigation only after the page's own scripts have run,
/// whatever `waitUntil` says. Measured in production on 2026-11-15, Amazon's
/// product and search pages took 10 to 14 seconds, and 26 to 33 for the first
/// page after the bot check cleared. A shorter bound fails every call: the
/// timeout discards the browser and its cleared cookies, so the next call
/// meets the bot check and the same slow first page again.
const NAVIGATE_TIMEOUT: Duration = Duration::from_secs(60);

/// One browser page.
pub trait Browser: Send {
    fn navigate<'a>(&'a mut self, url: &'a str) -> BoxFuture<'a, Result<(), AmazonError>>;
    /// The expression's result as text (a returned string is unquoted).
    fn evaluate<'a>(
        &'a mut self,
        expression: &'a str,
        timeout: Duration,
    ) -> BoxFuture<'a, Result<String, AmazonError>>;
}

/// Starts browsers.
pub trait Launcher: Send + Sync {
    fn launch(&self) -> BoxFuture<'_, Result<Box<dyn Browser>, AmazonError>>;
}

/// Launches `program mcp --stealth`.
pub struct ObscuraLauncher {
    program: String,
}

impl ObscuraLauncher {
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
        }
    }
}

impl Launcher for ObscuraLauncher {
    fn launch(&self) -> BoxFuture<'_, Result<Box<dyn Browser>, AmazonError>> {
        Box::pin(async move {
            let browser = ObscuraBrowser::start(&self.program).await?;
            Ok(Box::new(browser) as Box<dyn Browser>)
        })
    }
}

struct ObscuraBrowser {
    // Held so `kill_on_drop` ends the process with the browser.
    _child: Child,
    channel: Channel<ChildStdin, ChildStdout>,
}

/// The JSON-RPC exchange over the browser's pipes.
struct Channel<W, R> {
    stdin: W,
    stdout: BufReader<R>,
    next_id: i64,
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
    async fn start(program: &str) -> Result<Self, AmazonError> {
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
            .map_err(|e| AmazonError::Unavailable(format!("could not start {program}: {e}")))?;
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            return Err(AmazonError::Unavailable(
                "the browser's pipes are unavailable".into(),
            ));
        };
        let mut channel = Channel::new(stdin, stdout);
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
}

impl<W: AsyncWrite + Unpin + Send, R: AsyncRead + Unpin + Send> Channel<W, R> {
    fn new(stdin: W, stdout: R) -> Self {
        Self {
            stdin,
            stdout: BufReader::new(stdout),
            next_id: 1,
        }
    }

    async fn write(&mut self, message: &Value) -> Result<(), AmazonError> {
        let mut line = serde_json::to_vec(message)
            .map_err(|e| AmazonError::Browser(format!("could not encode a request: {e}")))?;
        line.push(b'\n');
        self.stdin
            .write_all(&line)
            .await
            .map_err(|e| AmazonError::Browser(format!("the browser stopped: {e}")))?;
        self.stdin
            .flush()
            .await
            .map_err(|e| AmazonError::Browser(format!("the browser stopped: {e}")))
    }

    async fn request(
        &mut self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, AmazonError> {
        let id = self.next_id;
        self.next_id += 1;
        // A tool call is named by its tool, so a timeout says which step hung.
        let label = params
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or(method)
            .to_owned();
        let exchange = async {
            self.write(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
                .await?;
            loop {
                let mut line = Vec::new();
                let read = (&mut self.stdout)
                    .take(MAX_LINE_BYTES)
                    .read_until(b'\n', &mut line)
                    .await
                    .map_err(|e| AmazonError::Browser(format!("the browser stopped: {e}")))?;
                if read == 0 {
                    return Err(AmazonError::Browser("the browser exited".into()));
                }
                if !line.ends_with(b"\n") {
                    return Err(AmazonError::Browser(
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
                    return Err(AmazonError::Browser(format!("{method} failed: {text}")));
                }
                return Ok(message.get("result").cloned().unwrap_or(Value::Null));
            }
        };
        tokio::time::timeout(timeout, exchange).await.map_err(|_| {
            AmazonError::Browser(format!(
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
    ) -> Result<String, AmazonError> {
        let result = self
            .request(
                "tools/call",
                json!({"name": tool, "arguments": arguments}),
                timeout,
            )
            .await?;
        let result: CallResult = serde_json::from_value(result)
            .map_err(|e| AmazonError::Browser(format!("{tool} returned an invalid result: {e}")))?;
        let text: String = result.content.into_iter().filter_map(|c| c.text).collect();
        if result.is_error {
            let first = text.lines().next().unwrap_or("unknown error");
            return Err(AmazonError::Browser(format!("{tool}: {first}")));
        }
        Ok(text)
    }

    async fn navigate(&mut self, url: &str) -> Result<(), AmazonError> {
        // Amazon renders results and product pages server-side; waiting
        // for every image and ad frame (`load`) takes longer still.
        let arguments = json!({"url": url, "waitUntil": "domcontentloaded"});
        self.call("browser_navigate", arguments, NAVIGATE_TIMEOUT)
            .await
            .map(drop)
    }

    async fn evaluate(
        &mut self,
        expression: &str,
        timeout: Duration,
    ) -> Result<String, AmazonError> {
        let millis = u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX);
        self.call(
            "browser_evaluate",
            json!({"expression": expression, "timeoutMs": millis}),
            timeout + Duration::from_secs(5),
        )
        .await
    }
}

impl Browser for ObscuraBrowser {
    fn navigate<'a>(&'a mut self, url: &'a str) -> BoxFuture<'a, Result<(), AmazonError>> {
        Box::pin(self.channel.navigate(url))
    }

    fn evaluate<'a>(
        &'a mut self,
        expression: &'a str,
        timeout: Duration,
    ) -> BoxFuture<'a, Result<String, AmazonError>> {
        Box::pin(self.channel.evaluate(expression, timeout))
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader, DuplexStream};

    use super::*;

    /// A browser that answers each request after `delay`, or never.
    fn browser_answering_after(delay: Option<Duration>) -> Channel<DuplexStream, DuplexStream> {
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
                    "result": {"content": [{"type": "text", "text": "Navigated"}]},
                });
                let mut line = serde_json::to_vec(&reply).unwrap();
                line.push(b'\n');
                replies.write_all(&line).await.unwrap();
            }
        });
        Channel::new(stdin, stdout)
    }

    #[tokio::test(start_paused = true)]
    async fn navigation_outlasts_amazons_slow_first_page_after_the_bot_check() {
        let mut browser = browser_answering_after(Some(Duration::from_secs(35)));
        let started = tokio::time::Instant::now();
        browser
            .navigate("https://www.amazon.ca/dp/B0CXDXP8VR")
            .await
            .unwrap();
        assert_eq!(started.elapsed(), Duration::from_secs(35));
    }

    #[tokio::test(start_paused = true)]
    async fn silent_browser_fails_at_the_navigation_bound_naming_the_step() {
        let mut browser = browser_answering_after(None);
        let started = tokio::time::Instant::now();
        let error = browser
            .navigate("https://www.amazon.ca/")
            .await
            .unwrap_err();
        assert_eq!(started.elapsed(), NAVIGATE_TIMEOUT);
        assert!(
            matches!(&error, AmazonError::Browser(text)
                if text == "browser_navigate got no answer within 60 seconds"),
            "{error}"
        );
    }
}
