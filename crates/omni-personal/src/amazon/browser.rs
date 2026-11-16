//! The Amazon session's browser: the shared Obscura client
//! (`omni_core::obscura`) behind the seam the tests replace.

use std::time::Duration;

use futures::future::BoxFuture;
use omni_core::obscura::{ObscuraBrowser, ObscuraError, ObscuraLimits};

use super::AmazonError;

/// Bound on one JSON-RPC line from the browser.
const MAX_LINE_BYTES: u64 = 4 * 1024 * 1024;
/// Obscura answers a navigation only after the page's own scripts have run,
/// whatever `waitUntil` says. Measured in production on 2026-11-15, Amazon's
/// product and search pages took 10 to 14 seconds, and 26 to 33 for the first
/// page after the bot check cleared. A shorter bound fails every call: the
/// timeout discards the browser and its cleared cookies, so the next call
/// meets the bot check and the same slow first page again.
const NAVIGATE_TIMEOUT: Duration = Duration::from_secs(60);

const LIMITS: ObscuraLimits = ObscuraLimits {
    max_line_bytes: MAX_LINE_BYTES,
    navigate_timeout: NAVIGATE_TIMEOUT,
};

impl From<ObscuraError> for AmazonError {
    fn from(error: ObscuraError) -> Self {
        match error {
            ObscuraError::Unavailable(text) => AmazonError::Unavailable(text),
            ObscuraError::Browser(text) => AmazonError::Browser(text),
        }
    }
}

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
            let browser = ObscuraBrowser::start(&self.program, LIMITS).await?;
            Ok(Box::new(browser) as Box<dyn Browser>)
        })
    }
}

impl Browser for ObscuraBrowser {
    fn navigate<'a>(&'a mut self, url: &'a str) -> BoxFuture<'a, Result<(), AmazonError>> {
        Box::pin(async move { Ok(ObscuraBrowser::navigate(self, url).await?) })
    }

    fn evaluate<'a>(
        &'a mut self,
        expression: &'a str,
        timeout: Duration,
    ) -> BoxFuture<'a, Result<String, AmazonError>> {
        Box::pin(async move { Ok(ObscuraBrowser::evaluate(self, expression, timeout).await?) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn navigation_bound_outlasts_amazons_slow_first_page_after_the_bot_check() {
        assert!(LIMITS.navigate_timeout >= Duration::from_secs(35));
    }
}
