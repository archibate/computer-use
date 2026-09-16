use std::{
    path::PathBuf,
    sync::{Arc, Mutex, MutexGuard, Weak},
    time::{Duration, Instant},
};

use cu_protocol::{
    CuError, DaemonRequest, DaemonResponse, ErrorCode, Rect, RequestEnvelope, ResponseEnvelope,
    ResponseResult, WaitOutcome, WaitRequest,
};
use schemars::JsonSchema;
use serde::Serialize;
use tokio::task::JoinHandle;
use uuid::Uuid;

use super::{
    FrameBinding, McpSessionState, signal::SignalStore, wait_error_invalidates_frame, with_recovery,
};
use crate::client;

const JOIN_TIMEOUT: Duration = Duration::from_secs(2);
const CANCELLED_MESSAGE: &str = "The wait was cancelled or superseded; call computer_observe for a current screenshot before continuing.";

#[derive(Debug, Serialize, JsonSchema)]
#[serde(tag = "status", rename_all = "snake_case")]
pub(super) enum McpWaitStarted {
    Started {
        signal_path: String,
        /// Guidance specific to this result.
        message: &'static str,
    },
}

#[derive(Debug, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum SignalResult {
    Changed {
        elapsed_ms: u64,
        activity_bbox: Rect,
        settled: bool,
        message: &'static str,
    },
    Timeout {
        elapsed_ms: u64,
    },
    Error {
        elapsed_ms: u64,
        code: String,
        message: String,
    },
    Cancelled {
        elapsed_ms: u64,
        message: &'static str,
    },
}

impl SignalResult {
    fn from_response(response: anyhow::Result<ResponseEnvelope>, elapsed_ms: u64) -> (Self, bool) {
        match response {
            Ok(ResponseEnvelope {
                result: ResponseResult::Ok(DaemonResponse::Wait(outcome)),
                ..
            }) => match outcome {
                WaitOutcome::Timeout { elapsed_ms } => (Self::Timeout { elapsed_ms }, false),
                WaitOutcome::Changed {
                    elapsed_ms,
                    activity_bbox,
                    observation,
                } => (
                    Self::Changed {
                        elapsed_ms,
                        activity_bbox,
                        settled: observation.settled,
                        message: "Monitored activity was detected; call computer_observe to inspect the current desktop before continuing.",
                    },
                    true,
                ),
            },
            Ok(ResponseEnvelope {
                result: ResponseResult::Error(error),
                ..
            }) => {
                if matches!(error.code, ErrorCode::StaleFrame | ErrorCode::Cancelled) {
                    return (
                        Self::Cancelled {
                            elapsed_ms,
                            message: CANCELLED_MESSAGE,
                        },
                        true,
                    );
                }
                let invalidates = wait_error_invalidates_frame(error.code);
                let error = with_recovery(error);
                (
                    Self::error(elapsed_ms, error.code.to_string(), error.message),
                    invalidates,
                )
            }
            Ok(_) => (
                Self::error(
                    elapsed_ms,
                    "internal",
                    "daemon returned the wrong wait response; call computer_observe before continuing",
                ),
                true,
            ),
            Err(error) => (
                Self::error(
                    elapsed_ms,
                    "daemon_unavailable",
                    format!("{error}; call computer_observe before continuing"),
                ),
                true,
            ),
        }
    }

    fn error(elapsed_ms: u64, code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Error {
            elapsed_ms,
            code: code.into(),
            message: message.into(),
        }
    }
}

struct Pending {
    generation: Uuid,
    baseline: String,
    started: Instant,
    signal: super::signal::PreparedSignal,
    task: JoinHandle<()>,
}

struct Control {
    closed: bool,
    store: SignalStore,
    pending: Option<Pending>,
    stopped_task: Option<JoinHandle<()>>,
}

impl Control {
    fn publish(&mut self, pending: Pending, result: &SignalResult) -> JoinHandle<()> {
        if let Err(error) = self.store.publish(pending.signal, result) {
            eprintln!("async wait result publication failed: {error}");
        }
        pending.task
    }

    fn cancel(&mut self) -> Option<JoinHandle<()>> {
        let pending = self.pending.take()?;
        let result = SignalResult::Cancelled {
            elapsed_ms: elapsed_millis(pending.started),
            message: CANCELLED_MESSAGE,
        };
        let task = self.publish(pending, &result);
        task.abort();
        Some(task)
    }
}

/// The gate never spans an await or desktop operation. It also protects cleanup
/// against a terminal writer when the MCP's asynchronous frame mutex is busy.
#[derive(Clone)]
pub(super) struct AsyncWaits {
    control: Arc<Mutex<Control>>,
}

impl AsyncWaits {
    pub(super) fn new(runtime_dir: PathBuf, session_id: Uuid) -> Self {
        Self {
            control: Arc::new(Mutex::new(Control {
                closed: false,
                store: SignalStore::new(runtime_dir, session_id),
                pending: None,
                stopped_task: None,
            })),
        }
    }

    pub(super) fn owner(&self) -> AsyncWaitOwner {
        AsyncWaitOwner(self.clone())
    }

    /// The caller holds the frame mutex through registration and its final
    /// request-cancellation check. The worker has no MCP request lifetime.
    pub(super) fn start(
        &self,
        socket: PathBuf,
        request: WaitRequest,
        baseline: FrameBinding,
        state: Arc<tokio::sync::Mutex<McpSessionState>>,
    ) -> Result<McpWaitStarted, CuError> {
        let mut control = lock(&self.control);
        if control.closed {
            return Err(CuError::new(ErrorCode::Cancelled, "MCP session is closing"));
        }
        if control.pending.is_some() {
            return Err(CuError::new(
                ErrorCode::Busy,
                "an asynchronous wait is already active",
            ));
        }
        let signal = control.store.prepare()?;
        let signal_path = signal.path().to_string_lossy().into_owned();
        let generation = Uuid::new_v4();
        let started = Instant::now();
        let guard = WorkerGuard {
            control: Arc::downgrade(&self.control),
            generation,
        };
        let baseline_id = baseline.internal_id.clone();
        let task = tokio::spawn(async move {
            // Created before spawn so even an unpolled task has a cleanup guard.
            let guard = guard;
            let response = client::request(
                &socket,
                &RequestEnvelope {
                    request_id: generation.to_string(),
                    request: DaemonRequest::Wait(request),
                },
            )
            .await;
            let (result, invalidates) =
                SignalResult::from_response(response, elapsed_millis(started));
            let mut state = state.lock().await;
            if invalidates {
                state.clear_if_current(&baseline);
            }
            guard.finish(&result);
        });
        control.pending = Some(Pending {
            generation,
            baseline: baseline_id,
            started,
            signal,
            task,
        });
        Ok(McpWaitStarted::Started {
            signal_path,
            message: "Use a monitor to notify when signal_path appears, then read its JSON result.",
        })
    }

    pub(super) async fn cancel(&self) {
        let task = lock(&self.control).cancel();
        join_cancelled(task).await;
    }

    pub(super) async fn cancel_if_superseded(&self, current: &str) {
        let task = {
            let mut control = lock(&self.control);
            if control
                .pending
                .as_ref()
                .is_some_and(|pending| pending.baseline != current)
            {
                control.cancel()
            } else {
                None
            }
        };
        join_cancelled(task).await;
    }

    /// Called directly when the transport detects EOF or a write error, before
    /// rmcp drains in-flight handlers. Keep the aborted handle for the owner.
    pub(super) fn stop(&self) {
        let mut control = lock(&self.control);
        if control.closed {
            return;
        }
        control.closed = true;
        // Publish cancellation before closing storage, even when the client is
        // gone. The result outlives this MCP process for a late monitor.
        control.stopped_task = control.cancel();
        control.store.cleanup();
    }

    fn close(&self) -> Option<JoinHandle<()>> {
        self.stop();
        lock(&self.control).stopped_task.take()
    }
}

/// Kept outside rmcp's service clones, so a worker cannot keep its owner alive.
pub(super) struct AsyncWaitOwner(AsyncWaits);

impl AsyncWaitOwner {
    pub(super) async fn shutdown(&self) {
        join_cancelled(self.0.close()).await;
    }
}

impl Drop for AsyncWaitOwner {
    fn drop(&mut self) {
        self.0.close();
    }
}

struct WorkerGuard {
    control: Weak<Mutex<Control>>,
    generation: Uuid,
}

impl WorkerGuard {
    fn finish(&self, result: &SignalResult) {
        let Some(control) = self.control.upgrade() else {
            return;
        };
        let mut control = lock(&control);
        if !control.closed
            && control
                .pending
                .as_ref()
                .is_some_and(|pending| pending.generation == self.generation)
        {
            let pending = control.pending.take().expect("generation was checked");
            // Dropping this handle after publication detaches only the finishing
            // task; its connection has already closed and it has no further I/O.
            drop(control.publish(pending, result));
        }
    }
}

impl Drop for WorkerGuard {
    fn drop(&mut self) {
        let Some(control) = self.control.upgrade() else {
            return;
        };
        let mut control = lock(&control);
        if !control.closed
            && control
                .pending
                .as_ref()
                .is_some_and(|pending| pending.generation == self.generation)
        {
            let pending = control.pending.take().expect("generation was checked");
            let result = SignalResult::error(
                elapsed_millis(pending.started),
                "internal",
                "asynchronous wait worker stopped unexpectedly; call computer_observe before starting another wait",
            );
            drop(control.publish(pending, &result));
        }
    }
}

fn lock(control: &Mutex<Control>) -> MutexGuard<'_, Control> {
    control
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn elapsed_millis(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

async fn join_cancelled(task: Option<JoinHandle<()>>) {
    if let Some(task) = task
        && tokio::time::timeout(JOIN_TIMEOUT, task).await.is_err()
    {
        eprintln!("async wait worker cleanup timed out");
    }
}

#[cfg(test)]
mod tests;
