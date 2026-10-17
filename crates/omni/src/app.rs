//! Process entry: configuration, logging, the context, and each command.

use std::io::Write as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use omni_alerts::{AlertGate, AlertLayer};
use omni_config::Config;
use omni_core::clock::{SharedClock, SystemClock};
use omni_http::SideEffectMode;
use omni_runtime::AppContext;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use crate::boot::{self, BootTrace, ServeOptions};
use crate::cli::{Command, USAGE};
use crate::context::{Foundation, app_paths, build_context};
use crate::logging::{self, LoggingSetup};

const LOG: &str = "Main";
/// Exit status after an interrupt (SIGINT/SIGTERM), as `runMain`.
pub const EXIT_INTERRUPTED: u8 = 130;
pub const EXIT_FAILURE: u8 = 1;
/// How long queued ERROR alerts may still go out after shutdown begins.
const ALERT_DRAIN: Duration = Duration::from_secs(3);

fn stderr_line(text: &str) {
    let _ = writeln!(std::io::stderr().lock(), "{text}");
}

fn stdout_text(text: &str) {
    let _ = write!(std::io::stdout().lock(), "{text}");
}

/// Runs `command` to completion and returns the exit status.
pub async fn main(command: Command) -> u8 {
    match command {
        Command::Help => {
            stdout_text(USAGE);
            0
        }
        Command::Healthcheck => healthcheck().await,
        Command::Doctor { image } => doctor(image).await,
        Command::CompatAudit(args) => compat_audit(&args),
        Command::Stylist(args) => match crate::stylist_cli::run(&args).await {
            Ok(report) => {
                stdout_text(&report);
                0
            }
            Err(error) => {
                stderr_line(&format!("stylist command failed: {error:#}"));
                EXIT_FAILURE
            }
        },
        Command::RunTaskUsage => {
            stderr_line("Usage: --run-task <TaskName>");
            EXIT_FAILURE
        }
        Command::RunTask { name, side_effects } => {
            let web_dist = crate::context::DEFAULT_WEB_DIST.into();
            with_app(
                side_effects,
                web_dist,
                |ctx, _gates, _interrupted| async move {
                    let booted_at = ctx.clock.now_ms();
                    let wired = crate::wiring::wire(&ctx, booted_at)
                        .await
                        .map_err(|e| boot::AppError::Other(e.to_string()))?;
                    let found = boot::run_task_once(&ctx, wired.subsystems, &name).await?;
                    Ok(if found { 0 } else { EXIT_FAILURE })
                },
            )
            .await
        }
        Command::Serve(serve) => {
            let options = ServeOptions {
                server_only: serve.server_only,
                web_dist: serve.web_dist.clone(),
            };
            with_app(
                serve.side_effects,
                serve.web_dist,
                |ctx, gates, interrupted| async move {
                    let listener = bind(&ctx.config).await?;
                    let booted_at = ctx.clock.now_ms();
                    let wired = crate::wiring::wire(&ctx, booted_at)
                        .await
                        .map_err(|e| boot::AppError::Other(e.to_string()))?;
                    boot::run(
                        ctx,
                        wired.subsystems,
                        options,
                        listener,
                        gates,
                        BootTrace::default(),
                    )
                    .await?;
                    Ok(if interrupted.load(Ordering::SeqCst) {
                        EXIT_INTERRUPTED
                    } else {
                        0
                    })
                },
            )
            .await
        }
        Command::Preview { port, web_dist } => crate::preview::main(port, web_dist).await,
    }
}

/// `FRONTEND_PORT` as a TCP port.
pub fn frontend_port(config: &Config) -> Result<u16, boot::AppError> {
    let port = config.frontend_port;
    if port.fract() == 0.0 && (0.0..=65535.0).contains(&port) {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        Ok(port as u16)
    } else {
        Err(boot::AppError::Other(format!(
            "invalid FRONTEND_PORT {port}"
        )))
    }
}

async fn bind(config: &Config) -> Result<TcpListener, boot::AppError> {
    let port = frontend_port(config)?;
    TcpListener::bind(("0.0.0.0", port))
        .await
        .map_err(|e| boot::AppError::Other(format!("listen on port {port}: {e}")))
}

fn load_config() -> Result<Arc<Config>, u8> {
    Config::from_process_env().map(Arc::new).map_err(|error| {
        stderr_line(&format!("Invalid configuration: {error}"));
        EXIT_FAILURE
    })
}

/// Cancels `token` on SIGINT or SIGTERM and records that it did.
fn watch_signals(
    token: CancellationToken,
    interrupted: Arc<AtomicBool>,
    tracker: &tokio_util::task::TaskTracker,
) {
    omni_core::spawn::spawn_tracked(tracker, "signals", async move {
        let terminate = async {
            #[cfg(unix)]
            {
                match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                    Ok(mut signal) => {
                        signal.recv().await;
                    }
                    Err(_) => std::future::pending::<()>().await,
                }
            }
            #[cfg(not(unix))]
            std::future::pending::<()>().await;
        };
        tokio::select! {
            () = token.cancelled() => return,
            _ = tokio::signal::ctrl_c() => {}
            () = terminate => {}
        }
        interrupted.store(true, Ordering::SeqCst);
        tracing::info!(target: LOG, "Interrupted; shutting down");
        token.cancel();
    });
}

/// Config, logging (console, file, run logs, alerts) and the context, then `body`.
async fn with_app<F, Fut>(side_effects: SideEffectMode, web_dist: std::path::PathBuf, body: F) -> u8
where
    F: FnOnce(AppContext, Arc<RwLock<Vec<Arc<dyn AlertGate>>>>, Arc<AtomicBool>) -> Fut,
    Fut: std::future::Future<Output = Result<u8, boot::AppError>>,
{
    let config = match load_config() {
        Ok(config) => config,
        Err(code) => return code,
    };
    let clock: SharedClock = Arc::new(SystemClock);
    let foundation = match Foundation::new(config.clone(), clock.clone(), side_effects) {
        Ok(foundation) => foundation,
        Err(error) => {
            stderr_line(&format!("HTTP client setup failed: {error}"));
            return EXIT_FAILURE;
        }
    };
    let gates: Arc<RwLock<Vec<Arc<dyn AlertGate>>>> = Arc::default();
    let (alerts, worker) =
        AlertLayer::new(foundation.pushover.clone(), gates.clone(), clock.clone());
    let tz = jiff::tz::TimeZone::get(&config.tz).unwrap_or(jiff::tz::TimeZone::UTC);
    let setup = LoggingSetup {
        level: config.log_level,
        logs_path: config
            .logs_path
            .as_deref()
            .filter(|p| !p.is_empty())
            .map(std::path::PathBuf::from),
        tz,
        clock: clock.clone(),
    };
    if logging::install(setup, Some(foundation.run_logs.clone()), Some(alerts)).is_err() {
        stderr_line("A global tracing subscriber was already installed");
    }
    let summary: serde_json::Map<String, serde_json::Value> = config
        .redacted_summary()
        .into_iter()
        .map(|(k, v)| (k.to_owned(), serde_json::Value::String(v)))
        .collect();
    tracing::info!(
        target: LOG,
        "Config: {}",
        omni_core::js::json_stringify(&serde_json::Value::Object(summary))
    );
    if side_effects == SideEffectMode::Record {
        tracing::warn!(target: LOG, "Side effects are recorded, not sent (shadow mode)");
    }

    let paths = app_paths(&config, web_dist);
    let ctx = match build_context(&foundation, &config.db_path(), paths).await {
        Ok(ctx) => ctx,
        Err(error) => {
            tracing::error!(target: LOG, error = %error, "Failed to open the docstore");
            return EXIT_FAILURE;
        }
    };
    let shutdown = ctx.shutdown.clone();
    omni_core::spawn::spawn_tracked(&ctx.tracker, "alert-worker", async move {
        tokio::select! {
            () = worker.run() => {}
            () = async {
                shutdown.cancelled().await;
                tokio::time::sleep(ALERT_DRAIN).await;
            } => {}
        }
    });
    let interrupted = Arc::new(AtomicBool::new(false));
    watch_signals(ctx.shutdown.clone(), interrupted.clone(), &ctx.tracker);
    let shutdown = ctx.shutdown.clone();
    let tracker = ctx.tracker.clone();
    let code = match body(ctx, gates, interrupted.clone()).await {
        Ok(code) => code,
        Err(error) => {
            tracing::error!(target: LOG, error = %error, "Fatal error");
            if interrupted.load(Ordering::SeqCst) {
                EXIT_INTERRUPTED
            } else {
                EXIT_FAILURE
            }
        }
    };
    shutdown.cancel();
    tracker.close();
    let _ = tokio::time::timeout(boot::SHUTDOWN_BOUND, tracker.wait()).await;
    code
}

/// `GET /api/health` on `FRONTEND_PORT`.
async fn healthcheck() -> u8 {
    let config = match load_config() {
        Ok(config) => config,
        Err(code) => return code,
    };
    let port = match frontend_port(&config) {
        Ok(port) => port,
        Err(error) => {
            stderr_line(&error.to_string());
            return EXIT_FAILURE;
        }
    };
    match check_health(port).await {
        Ok(()) => 0,
        Err(error) => {
            stderr_line(&format!("unhealthy: {error}"));
            EXIT_FAILURE
        }
    }
}

/// `GET http://127.0.0.1:<port>/api/health` must answer `{"status":"ok"}`.
pub async fn check_health(port: u16) -> Result<(), String> {
    let http =
        omni_http::HttpClient::new(omni_http::HttpConfig::default()).map_err(|e| e.to_string())?;
    let url = omni_http::Url::parse(&format!("http://127.0.0.1:{port}/api/health"))
        .map_err(|e| e.to_string())?;
    let response = http
        .request(omni_http::Method::GET, url)
        .timeout(Duration::from_secs(5))
        .send_bounded(4096)
        .await
        .map_err(|e| e.to_string())?;
    if !response.status.is_success() {
        return Err(format!("status {}", response.status));
    }
    let body: serde_json::Value =
        serde_json::from_slice(&response.body).map_err(|e| e.to_string())?;
    if body.get("status").and_then(serde_json::Value::as_str) == Some("ok") {
        Ok(())
    } else {
        Err(format!("unexpected body {body}"))
    }
}

async fn doctor(image: bool) -> u8 {
    let env = crate::doctor::DoctorEnv::from_process_env();
    let checks = crate::doctor::checks(&env, std::path::Path::new("assets")).await;
    stdout_text(&crate::doctor::report(&checks));
    let failed = checks.iter().filter(|c| !c.ok).count();
    if failed == 0 {
        0
    } else {
        stderr_line(&format!(
            "doctor: {failed} check(s) failed{}",
            if image { " in the image" } else { "" }
        ));
        EXIT_FAILURE
    }
}

fn compat_audit(args: &crate::compat_audit::AuditArgs) -> u8 {
    let now = omni_core::clock::Clock::now_ms(&SystemClock);
    match crate::compat_audit::run(args, now) {
        Ok((clean, report)) => {
            stdout_text(&report);
            if clean { 0 } else { EXIT_FAILURE }
        }
        Err(error) => {
            stderr_line(&format!("compat-audit failed: {error:#}"));
            EXIT_FAILURE
        }
    }
}
