//! Run logs: `GET /api/task-runs/:runId/logs` and its live SSE tail.
//!
//! The stream sends an `init` frame with everything buffered so far, `line`
//! frames as the task logs, and a `done` frame with the settled run, then
//! ends. For a finished run `init` and `done` arrive back to back. Reconnects
//! are safe: `init` re-sends the full buffer and the client replaces its
//! state. Ids count from 0 per stream; `ping` frames every 25 s carry no id.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use omni_runtime::AppContext;
use omni_store::{LogLine, StoreError};
use omni_tasks::RunLogEvent;
use omni_tasks::persistence::{TaskRunData, TaskRunStatus, get_run, get_run_logs};
use serde_json::{Value, json};
use tokio::sync::{broadcast, mpsc};
use tokio_stream::wrappers::UnboundedReceiverStream;

use super::OpsState;
use super::dashboard::{SSE_HEARTBEAT, until_shutdown, with_sse_headers};
use crate::json::{error, js_json};

fn run_json(run: &TaskRunData) -> Value {
    serde_json::to_value(omni_api::runs::Run::from(run)).unwrap_or(Value::Null)
}

fn lines_json(lines: &[LogLine]) -> Value {
    serde_json::to_value(lines).unwrap_or(Value::Array(Vec::new()))
}

fn stringify(value: &Value) -> String {
    omni_core::js::json_stringify(&omni_core::js::normalize_numbers(value.clone()))
}

/// The live buffer of an in-flight run, else the persisted
/// row, else nothing.
async fn collect(ctx: &AppContext, run_id: &str) -> Result<(Vec<LogLine>, u64), StoreError> {
    if let Some(active) = ctx.run_logs().active(run_id) {
        return Ok(active);
    }
    Ok(get_run_logs(&ctx.store, run_id)
        .await?
        .map_or_else(|| (Vec::new(), 0), |logs| (logs.lines, logs.dropped)))
}

pub async fn logs(State(state): State<OpsState>, Path(run_id): Path<String>) -> Response {
    let ctx = &state.ctx;
    let run = match get_run(&ctx.store, &run_id).await {
        Ok(Some(run)) => run,
        Ok(None) => return error(StatusCode::NOT_FOUND, "Unknown run"),
        Err(e) => return omni_server_kit::ApiError::internal(e).into_response(),
    };
    match collect(ctx, &run_id).await {
        Ok((lines, dropped)) => js_json(
            StatusCode::OK,
            &json!({ "run": run_json(&run), "lines": lines_json(&lines), "dropped": dropped }),
        ),
        Err(e) => omni_server_kit::ApiError::internal(e).into_response(),
    }
}

struct Frames {
    tx: mpsc::UnboundedSender<Event>,
    next_id: u64,
}

impl Frames {
    fn send(&mut self, name: &str, data: &str) -> bool {
        let event = omni_server_kit::sse::event(name, self.next_id, data);
        self.next_id += 1;
        self.tx.send(event).is_ok()
    }
}

async fn forward(
    ctx: AppContext,
    run_id: String,
    run: TaskRunData,
    mut frames: Frames,
    mut events: broadcast::Receiver<RunLogEvent>,
) {
    use broadcast::error::RecvError;
    loop {
        let event = tokio::select! {
            () = ctx.shutdown.cancelled() => return,
            () = frames.tx.closed() => return,
            event = events.recv() => event,
        };
        match event {
            Ok(RunLogEvent::Line { run_id: id, line }) if id == run_id => {
                let data = stringify(&serde_json::to_value(&line).unwrap_or(Value::Null));
                if !frames.send("line", &data) {
                    return;
                }
            }
            Ok(RunLogEvent::End { run_id: id }) if id == run_id => {
                let latest = get_run(&ctx.store, &run_id).await.ok().flatten();
                let done = latest.as_ref().unwrap_or(&run);
                frames.send("done", &stringify(&run_json(done)));
                return;
            }
            Ok(_) | Err(RecvError::Lagged(_)) => {}
            Err(RecvError::Closed) => return,
        }
    }
}

pub async fn stream(State(state): State<OpsState>, Path(run_id): Path<String>) -> Response {
    let ctx = state.ctx.clone();
    // Subscribe before reading the run: its end is persisted before the `end`
    // event, so a run read as running always delivers that event here.
    let mut events = ctx.bus.run_logs();
    let run = match get_run(&ctx.store, &run_id).await {
        Ok(Some(run)) => run,
        Ok(None) => return error(StatusCode::NOT_FOUND, "Unknown run"),
        Err(e) => return omni_server_kit::ApiError::internal(e).into_response(),
    };
    // Lines queued so far are already in the buffer `init` replays; drop them
    // right before reading it (a line logged in between may repeat after
    // `init`, never go missing) but remember an `end`.
    let mut ended = false;
    loop {
        match events.try_recv() {
            Ok(RunLogEvent::End { run_id: id }) if id == run_id => ended = true,
            Ok(_) | Err(broadcast::error::TryRecvError::Lagged(_)) => {}
            Err(_) => break,
        }
    }
    let (lines, dropped) = match collect(&ctx, &run_id).await {
        Ok(collected) => collected,
        Err(e) => return omni_server_kit::ApiError::internal(e).into_response(),
    };
    let (tx, rx) = mpsc::unbounded_channel();
    let mut frames = Frames { tx, next_id: 0 };
    frames.send(
        "init",
        &stringify(
            &json!({ "run": run_json(&run), "lines": lines_json(&lines), "dropped": dropped }),
        ),
    );
    if run.status != TaskRunStatus::Running {
        frames.send("done", &stringify(&run_json(&run)));
    } else if ended {
        let latest = get_run(&ctx.store, &run_id).await.ok().flatten();
        frames.send(
            "done",
            &stringify(&run_json(latest.as_ref().unwrap_or(&run))),
        );
    } else {
        let forwarder = forward(ctx.clone(), run_id, run, frames, events);
        omni_core::spawn::spawn_tracked(&ctx.tracker, "run-log-stream", forwarder);
    }
    let stream = omni_server_kit::sse::with_ping(
        UnboundedReceiverStream::new(rx),
        SSE_HEARTBEAT,
        ctx.clock.clone(),
    );
    with_sse_headers(Sse::new(until_shutdown(stream, ctx.shutdown.clone())))
}
