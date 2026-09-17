mod signal;
mod transport;
mod wait;

use std::{
    collections::{HashMap, VecDeque},
    path::{Path, PathBuf},
    str::FromStr,
    sync::Arc,
    time::Duration,
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use cu_protocol::{
    ActOutcome, ActRequest, ActStatus, Action, CuError, DEFAULT_WAIT_QUIET_MS,
    DEFAULT_WAIT_TIMEOUT_MS, DaemonRequest, DaemonResponse, ErrorCode, Observation, ObserveRequest,
    Rect, RequestEnvelope, ResponseEnvelope, ResponseResult, SettlePolicy, WaitOutcome,
    WaitRequest,
};
use rmcp::{
    ErrorData as McpError, RoleServer, ServerHandler, ServiceExt,
    handler::server::{tool::RequestId, wrapper::Parameters},
    model::{
        CallToolResult, ContentBlock, Implementation, ProgressNotificationParam,
        ServerCapabilities, ServerInfo,
    },
    service::{NotificationContext, RequestContext},
    tool, tool_handler, tool_router,
    transport::{async_rw::AsyncRwTransport, stdio},
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::{sync::Mutex, time::MissedTickBehavior};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::{client, desktop::PrivateDesktop};
use wait::{AsyncWaits, McpWaitStarted};

const MCP_INSTRUCTIONS: &str = "Call computer_connect to select a desktop before using other tools. Use computer_observe before the first action, after connecting, and whenever the current screenshot is unknown. Pass the latest returned frame number to computer_act.frame or computer_wait.frame; x and y are integer pixels in [0,width) and [0,height). computer_act affects the live desktop and returns a fresh observation. Batch actions only when no intermediate inspection is needed. Apply your authorization policy before consequential UI actions.";
const PROFILE_FETCH_TIMEOUT: Duration = Duration::from_secs(5);
const WAIT_PROGRESS_INTERVAL: Duration = Duration::from_secs(5);
const MAX_MCP_CACHED_ACTIONS: usize = 64;

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct McpConnectRequest {
    /// @default (the default when omitted) connects to the existing default daemon.
    /// @private creates or reuses this MCP process's session-local offscreen Xvfb/Openbox desktop;
    /// switching away preserves it until this MCP process exits.
    /// Other values select existing named daemons, including literal names default and private.
    /// Instance names use 1-64 ASCII letters, digits, dots, underscores, or hyphens; . and .. are invalid.
    /// Find candidate names with: ls "${XDG_RUNTIME_DIR:-/run/user/$(id -u)}/computer-use/instances".
    #[serde(default = "default_connect_name")]
    #[schemars(
        length(min = 1, max = 64),
        regex(pattern = "^(@default|@private|[A-Za-z0-9_.-]+)$")
    )]
    name: String,
}

fn default_connect_name() -> String {
    "@default".to_owned()
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ConnectionTarget {
    Default,
    Private,
    Named(crate::InstanceName),
}

impl FromStr for ConnectionTarget {
    type Err = String;

    fn from_str(name: &str) -> Result<Self, Self::Err> {
        match name {
            "@default" => Ok(Self::Default),
            "@private" => Ok(Self::Private),
            _ => name.parse().map(Self::Named).map_err(|error| {
                format!(
                    "invalid name: use @default, @private, or an existing instance name; {error}"
                )
            }),
        }
    }
}

#[derive(Debug, Serialize, JsonSchema)]
struct McpConnection {
    /// Connection details and trusted operating guidance for this desktop.
    profile: String,
}

#[derive(Debug, Clone)]
struct Connection {
    socket: PathBuf,
    target: ConnectionTarget,
    generation: Uuid,
    cancelled: CancellationToken,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[schemars(title = "computer action input")]
struct McpActRequest {
    /// Session-local frame number from the latest observe or action result.
    #[schemars(range(min = 1))]
    frame: u64,
    /// Between 1 and 16 sequential actions. Batch only when intermediate UI need not be inspected.
    #[schemars(length(min = 1, max = 16))]
    actions: Vec<Action>,
    /// Settling policy applied before capturing the post-action screenshot.
    #[serde(default)]
    settle: SettlePolicy,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[schemars(title = "computer wait input")]
struct McpWaitRequest {
    /// Session-local frame number from the latest observation or changed wait.
    #[schemars(range(min = 1))]
    frame: u64,
    /// Between 1 and 8 rectangles whose union is watched for exact pixel changes.
    #[schemars(length(min = 1, max = 8))]
    include_rects: Vec<Rect>,
    /// Up to 8 rectangles removed from the included region.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 8))]
    exclude_rects: Vec<Rect>,
    /// Total budget in milliseconds for detection and post-change settling.
    /// Defaults to 3600000 (1 hour); range 1-3600000.
    #[serde(default = "default_wait_timeout_ms")]
    #[schemars(range(min = 1, max = 3_600_000))]
    timeout_ms: u64,
    /// Required continuous stability after activity, in milliseconds.
    /// Defaults to 2000 (2 seconds); range 1-60000.
    #[serde(default = "default_wait_quiet_ms")]
    #[schemars(range(min = 1, max = 60_000))]
    quiet_ms: u64,
    /// Return `signal_path` immediately for background monitoring. Observe cancels pending waits.
    #[serde(default, rename = "async")]
    async_mode: bool,
}

const fn default_wait_timeout_ms() -> u64 {
    DEFAULT_WAIT_TIMEOUT_MS
}

const fn default_wait_quiet_ms() -> u64 {
    DEFAULT_WAIT_QUIET_MS
}

#[derive(Debug, Serialize, JsonSchema)]
struct McpObservation {
    /// Session-local frame number required by the next act or wait call.
    #[schemars(range(min = 1))]
    frame: u64,
    /// PNG width and exclusive upper bound for action x coordinates.
    width: u32,
    /// PNG height and exclusive upper bound for action y coordinates.
    height: u32,
    /// Whether the screen stayed unchanged for `quiet_ms`; an unsettled frame remains usable.
    settled: bool,
}

impl McpObservation {
    fn from_protocol(observation: &Observation, frame: u64) -> Self {
        Self {
            frame,
            width: observation.width,
            height: observation.height,
            settled: observation.settled,
        }
    }
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum McpWaitStatus {
    Changed,
    Timeout,
}

#[derive(Debug, Serialize, JsonSchema)]
struct McpWaitOutcome {
    /// Whether monitored activity was observed or the unchanged timeout elapsed.
    status: McpWaitStatus,
    /// Monotonic time through the terminal screen sample; excludes result encoding and transport.
    elapsed_ms: u64,
    /// Current session-local frame: unchanged on timeout and new after activity.
    #[schemars(range(min = 1))]
    frame: u64,
    /// Whether the terminal frame satisfied `quiet_ms`; always true on an unchanged timeout.
    settled: bool,
    /// Bounding box covering monitored activity since the first change.
    #[serde(skip_serializing_if = "Option::is_none")]
    activity_bbox: Option<Rect>,
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(untagged)]
#[schemars(extend("type" = "object"))]
enum McpWaitReply {
    Finished(McpWaitOutcome),
    Started(McpWaitStarted),
}

#[derive(Debug, Serialize, JsonSchema)]
struct McpActionError {
    /// Stable error category.
    #[schemars(with = "String")]
    code: ErrorCode,
    /// Human-readable diagnostic and recovery context.
    message: String,
    /// Number of leading actions that executed before this failure.
    #[schemars(range(max = 16))]
    executed: usize,
}

impl McpActionError {
    fn from_protocol(error: CuError, executed: usize) -> Self {
        Self {
            code: error.code,
            message: error.message,
            executed,
        }
    }
}

#[derive(Debug, Serialize, JsonSchema)]
struct McpActOutcome {
    /// Complete or partial execution status.
    status: ActStatus,
    /// Number of leading actions that executed successfully.
    #[schemars(range(max = 16))]
    executed: usize,
    /// Failure that stopped a partial batch; absent when status is `ok`.
    #[serde(skip_serializing_if = "Option::is_none")]
    action_error: Option<McpActionError>,
    /// Whether the cached result's screenshot has expired.
    #[serde(default, skip_serializing_if = "is_false")]
    image_expired: bool,
    /// Fresh post-action or post-failure frame, absent only when `image_expired` is true.
    #[serde(skip_serializing_if = "Option::is_none")]
    observation: Option<McpObservation>,
    /// Guidance specific to this result.
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<&'static str>,
}

#[allow(clippy::trivially_copy_pass_by_ref)]
const fn is_false(value: &bool) -> bool {
    !*value
}

impl McpActOutcome {
    fn from_protocol(outcome: ActOutcome, observation: Option<McpObservation>) -> Self {
        let ActOutcome {
            status,
            executed,
            action_error,
            image_expired,
            observation: _,
        } = outcome;
        Self {
            status,
            executed,
            action_error: action_error.map(|error| McpActionError::from_protocol(error, executed)),
            image_expired,
            observation,
            message: if image_expired {
                Some(
                    "This is a cached action result; the action was not repeated. Call computer_observe for a current screenshot before continuing; do not repeat completed actions.",
                )
            } else if status == ActStatus::Partial {
                Some(
                    "Inspect the returned observation before continuing; do not repeat completed actions.",
                )
            } else {
                None
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FrameBinding {
    frame: u64,
    internal_id: String,
}

#[derive(Debug, Clone)]
struct CachedMcpAction {
    request: McpActRequest,
    protocol_request: ActRequest,
    result_frame: Option<FrameBinding>,
}

#[derive(Debug, Default)]
struct McpSessionState {
    connection: Option<Connection>,
    latest_frame: u64,
    current: Option<FrameBinding>,
    completed_actions: HashMap<String, CachedMcpAction>,
    cache_order: VecDeque<String>,
}

impl McpSessionState {
    fn disconnect(&mut self) {
        if let Some(connection) = self.connection.take() {
            connection.cancelled.cancel();
        }
        self.current = None;
        self.completed_actions.clear();
        self.cache_order.clear();
    }

    fn resolve_frame(&self, frame: u64) -> Result<FrameBinding, CuError> {
        if frame == 0 {
            return Err(CuError::new(
                ErrorCode::InvalidAction,
                "frame must be a positive integer",
            ));
        }
        let current = self.current.as_ref().ok_or_else(|| {
            CuError::new(
                ErrorCode::StaleFrame,
                "no current screenshot in this MCP session; call computer_observe",
            )
        })?;
        if frame != current.frame {
            return Err(CuError::new(
                ErrorCode::StaleFrame,
                "the supplied frame is no longer current; call computer_observe",
            ));
        }
        Ok(current.clone())
    }

    fn prepare_action(
        &mut self,
        request_id: &str,
        request: &McpActRequest,
    ) -> Result<ActRequest, CuError> {
        if let Some(cached) = self.completed_actions.get(request_id) {
            if cached.request == *request {
                return Ok(cached.protocol_request.clone());
            }
            return Err(CuError::new(
                ErrorCode::ProtocolError,
                "request_id was reused with a different request",
            ));
        }
        let current = self.resolve_frame(request.frame)?;

        let protocol_request = ActRequest {
            expected_frame_id: current.internal_id,
            actions: request.actions.clone(),
            settle: request.settle,
        };
        self.cache_order.push_back(request_id.to_owned());
        self.completed_actions.insert(
            request_id.to_owned(),
            CachedMcpAction {
                request: request.clone(),
                protocol_request: protocol_request.clone(),
                result_frame: None,
            },
        );
        while self.cache_order.len() > MAX_MCP_CACHED_ACTIONS {
            if let Some(oldest) = self.cache_order.pop_front() {
                self.completed_actions.remove(&oldest);
            }
        }
        Ok(protocol_request)
    }

    fn bind_observation(&mut self, internal_id: String) -> Result<FrameBinding, CuError> {
        self.latest_frame = self
            .latest_frame
            .checked_add(1)
            .ok_or_else(|| CuError::new(ErrorCode::Internal, "MCP frame sequence was exhausted"))?;
        Ok(FrameBinding {
            frame: self.latest_frame,
            internal_id,
        })
    }

    fn bind_action_observation(
        &mut self,
        request_id: &str,
        internal_id: String,
    ) -> Result<FrameBinding, CuError> {
        if let Some(binding) = self
            .completed_actions
            .get(request_id)
            .and_then(|cached| cached.result_frame.as_ref())
        {
            if binding.internal_id == internal_id {
                return Ok(binding.clone());
            }
            return Err(CuError::new(
                ErrorCode::ProtocolError,
                "a cached action returned a different observation",
            ));
        }

        let binding = self.bind_observation(internal_id)?;
        let cached = self
            .completed_actions
            .get_mut(request_id)
            .ok_or_else(|| CuError::new(ErrorCode::Internal, "action request was not tracked"))?;
        cached.result_frame = Some(binding.clone());
        Ok(binding)
    }

    fn bind_wait_observation(
        &mut self,
        baseline: &FrameBinding,
        internal_id: String,
    ) -> Result<FrameBinding, CuError> {
        self.ensure_current(baseline)?;
        self.bind_observation(internal_id)
    }

    fn ensure_current(&self, expected: &FrameBinding) -> Result<(), CuError> {
        if self.current.as_ref() == Some(expected) {
            Ok(())
        } else {
            Err(CuError::new(
                ErrorCode::StaleFrame,
                "the wait frame was superseded; use the latest returned observation or observe again",
            ))
        }
    }

    fn commit(&mut self, binding: FrameBinding) {
        if binding.frame == self.latest_frame {
            self.current = Some(binding);
        }
    }

    fn clear(&mut self) {
        self.current = None;
    }

    fn clear_if_current(&mut self, expected: &FrameBinding) {
        if self.current.as_ref() == Some(expected) {
            self.clear();
        }
    }

    fn apply_error(&mut self, error: &CuError) {
        if !matches!(
            error.code,
            ErrorCode::InvalidAction
                | ErrorCode::OutOfBounds
                | ErrorCode::UnsupportedInput
                | ErrorCode::ProtocolError
        ) {
            self.clear();
        }
    }
}

fn wait_error_invalidates_frame(code: ErrorCode) -> bool {
    !matches!(
        code,
        ErrorCode::StaleFrame
            | ErrorCode::Busy
            | ErrorCode::InvalidAction
            | ErrorCode::OutOfBounds
            | ErrorCode::UnsupportedInput
            | ErrorCode::ProtocolError
    )
}

#[derive(Clone)]
pub struct ComputerUseMcp {
    session_id: Uuid,
    state: Arc<Mutex<McpSessionState>>,
    waits: AsyncWaits,
    desktop: PrivateDesktop,
    shutdown: CancellationToken,
    tool_router: rmcp::handler::server::tool::ToolRouter<Self>,
}

#[tool_router(router = tool_router)]
impl ComputerUseMcp {
    pub fn new() -> Self {
        let session_id = Uuid::new_v4();
        Self {
            session_id,
            state: Arc::new(Mutex::new(McpSessionState::default())),
            waits: AsyncWaits::new(crate::default_runtime_dir(), session_id),
            desktop: PrivateDesktop::default(),
            shutdown: CancellationToken::new(),
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        description = "Connect to a desktop. Every valid connection attempt cancels pending waits and invalidates previous frames; call computer_observe afterward.",
        output_schema = rmcp::handler::server::tool::schema_for_output::<McpConnection>(),
        annotations(title = "Connect computer", read_only_hint = false, destructive_hint = false, idempotent_hint = false, open_world_hint = true)
    )]
    async fn computer_connect(
        &self,
        Parameters(request): Parameters<McpConnectRequest>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let target = match request.name.parse::<ConnectionTarget>() {
            Ok(target) => target,
            Err(error) => return Ok(CallToolResult::error(vec![ContentBlock::text(error)])),
        };
        let private = matches!(&target, ConnectionTarget::Private);
        let mut state = self.state.lock().await;
        if context.ct.is_cancelled() || self.shutdown.is_cancelled() {
            return Ok(cancelled_connect_result());
        }
        state.disconnect();
        self.waits.cancel().await;
        let (socket, created) = match &target {
            ConnectionTarget::Default => (crate::default_runtime_dir().join("cu.sock"), false),
            ConnectionTarget::Named(name) => {
                (crate::named_instance_dir(name).join("cu.sock"), false)
            }
            ConnectionTarget::Private => match self.desktop.launch() {
                Ok(result) => result,
                Err(error) => return Ok(connection_error(&target, &error)),
            },
        };
        let connecting = async {
            if !private {
                return tokio::time::timeout(PROFILE_FETCH_TIMEOUT, fetch_profile(&socket))
                    .await
                    .map_err(|_| anyhow::anyhow!("daemon profile request timed out"))?;
            }
            tokio::time::timeout(crate::desktop::START_TIMEOUT + PROFILE_FETCH_TIMEOUT, async {
                loop {
                    if !self.desktop.running()? {
                        anyhow::bail!("private desktop stopped; check that Xvfb and Openbox are installed, then call computer_connect again");
                    }
                    match tokio::time::timeout(PROFILE_FETCH_TIMEOUT, fetch_profile(&socket)).await {
                        Ok(Ok(profile)) => return Ok(profile),
                        Ok(Err(_)) if created => tokio::time::sleep(Duration::from_millis(20)).await,
                        Ok(Err(error)) => return Err(error),
                        Err(_) => anyhow::bail!("private desktop profile request timed out"),
                    }
                }
            }).await.map_err(|_| anyhow::anyhow!("private desktop startup timed out"))?
        };
        let result = tokio::select! {
            biased;
            () = context.ct.cancelled() => Err(anyhow::anyhow!("connection cancelled")),
            () = self.shutdown.cancelled() => Err(anyhow::anyhow!("MCP session is closing")),
            result = connecting => result,
        };
        match result {
            Ok(profile) if !context.ct.is_cancelled() && !self.shutdown.is_cancelled() => {
                state.connection = Some(Connection {
                    socket,
                    target,
                    generation: Uuid::new_v4(),
                    cancelled: self.shutdown.child_token(),
                });
                Ok(structured_result(McpConnection {
                    profile: profile.unwrap_or_default(),
                }))
            }
            result => {
                if created {
                    self.desktop.reset().await;
                }
                if context.ct.is_cancelled() || self.shutdown.is_cancelled() {
                    Ok(cancelled_connect_result())
                } else {
                    Ok(connection_error(
                        &target,
                        &result.expect_err("successful connection handled above"),
                    ))
                }
            }
        }
    }

    #[tool(
        description = "Capture the current desktop after optional visual settling. Returns a session-local frame number, image dimensions, settling status, and a PNG. Call before the first action or whenever UI state is uncertain.",
        output_schema = rmcp::handler::server::tool::schema_for_output::<McpObservation>(),
        annotations(
            title = "Observe computer",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    async fn computer_observe(
        &self,
        Parameters(request): Parameters<ObserveRequest>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        if context.ct.is_cancelled() {
            return Ok(cancelled_result());
        }
        let mut state = self.state.lock().await;
        let connection = match connected(&state) {
            Ok(connection) => connection,
            Err(result) => return Ok(result),
        };
        if context.ct.is_cancelled() {
            return Ok(cancelled_result());
        }
        self.waits.cancel().await;
        let response = match self
            .request_daemon(
                &connection,
                Uuid::new_v4().to_string(),
                DaemonRequest::Observe(request),
            )
            .await
        {
            Ok(response) => response,
            Err(result) => {
                state.clear();
                return Ok(result);
            }
        };
        if context.ct.is_cancelled() {
            state.clear();
            return Ok(cancelled_result());
        }

        match response.result {
            ResponseResult::Error(error) => {
                state.clear();
                Ok(structured_error(&error))
            }
            ResponseResult::Ok(DaemonResponse::Observe(observation)) => {
                let binding = match state.bind_observation(observation.frame_id.clone()) {
                    Ok(binding) => binding,
                    Err(error) => {
                        state.clear();
                        return Ok(structured_error(&error));
                    }
                };
                let result = image_result(
                    McpObservation::from_protocol(&observation, binding.frame),
                    observation.image_path,
                )
                .await?;
                if context.ct.is_cancelled() || result.is_error == Some(true) {
                    state.clear();
                    if context.ct.is_cancelled() {
                        return Ok(cancelled_result());
                    }
                } else {
                    state.commit(binding);
                }
                Ok(result)
            }
            ResponseResult::Ok(_) => {
                state.clear();
                Ok(unexpected_response("observe"))
            }
        }
    }

    #[tool(
        description = "Wait for a change in selected screen regions. Blocks by default; prefer async:true for background monitoring. If changes repeat quickly, narrow or exclude noisy regions.",
        output_schema = rmcp::handler::server::tool::schema_for_output::<McpWaitReply>(),
        annotations(
            title = "Wait for computer change",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    async fn computer_wait(
        &self,
        Parameters(request): Parameters<McpWaitRequest>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        if context.ct.is_cancelled() {
            return Ok(cancelled_result());
        }
        let (baseline, connection) = {
            let state = self.state.lock().await;
            let connection = match connected(&state) {
                Ok(connection) => connection,
                Err(result) => return Ok(result),
            };
            match state.resolve_frame(request.frame) {
                Ok(binding) => (binding, connection),
                Err(error) => return Ok(structured_error(&error)),
            }
        };
        let protocol_request = WaitRequest {
            last_frame_id: baseline.internal_id.clone(),
            include_rects: request.include_rects,
            exclude_rects: request.exclude_rects,
            timeout_ms: request.timeout_ms,
            quiet_ms: request.quiet_ms,
        };
        if request.async_mode {
            return self
                .start_async_wait(protocol_request, baseline, &connection, &context)
                .await;
        }
        let response = match self
            .request_wait_daemon(&connection, protocol_request, &context)
            .await
        {
            Ok(response) => response,
            Err(result) => {
                self.state.lock().await.clear_if_current(&baseline);
                return Ok(result);
            }
        };
        if context.ct.is_cancelled() {
            self.state.lock().await.clear_if_current(&baseline);
            return Ok(cancelled_result());
        }

        match response.result {
            ResponseResult::Error(error) => {
                let mut state = self.state.lock().await;
                if wait_error_invalidates_frame(error.code) {
                    state.clear_if_current(&baseline);
                }
                if error.code == ErrorCode::StaleFrame {
                    Ok(stale_frame_result(
                        "the desktop was superseded while waiting; call computer_observe",
                    ))
                } else {
                    Ok(structured_error(&error))
                }
            }
            ResponseResult::Ok(DaemonResponse::Wait(outcome)) => {
                self.finish_wait(outcome, baseline, &context).await
            }
            ResponseResult::Ok(_) => {
                self.state.lock().await.clear_if_current(&baseline);
                Ok(unexpected_response("wait"))
            }
        }
    }

    #[tool(
        description = "Execute sequential input actions on the live desktop and return execution metadata, a fresh frame, and PNG. Batch only when no intermediate inspection is needed.",
        output_schema = rmcp::handler::server::tool::schema_for_output::<McpActOutcome>(),
        annotations(
            title = "Act on computer",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    async fn computer_act(
        &self,
        Parameters(request): Parameters<McpActRequest>,
        RequestId(request_id): RequestId,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        if context.ct.is_cancelled() {
            return Ok(cancelled_result());
        }
        let mut state = self.state.lock().await;
        let connection = match connected(&state) {
            Ok(connection) => connection,
            Err(result) => return Ok(result),
        };
        let request_id = self.action_request_id(&request_id, connection.generation);
        if context.ct.is_cancelled() {
            return Ok(cancelled_result());
        }
        let is_retry = state.completed_actions.contains_key(&request_id);
        let protocol_request = match state.prepare_action(&request_id, &request) {
            Ok(request) => request,
            Err(error) => {
                state.apply_error(&error);
                return Ok(structured_error(&error));
            }
        };
        let response = match self
            .request_daemon(
                &connection,
                request_id.clone(),
                DaemonRequest::Act(protocol_request),
            )
            .await
        {
            Ok(response) => response,
            Err(result) => {
                state.clear();
                return Ok(result);
            }
        };
        if context.ct.is_cancelled() {
            state.clear();
            return Ok(cancelled_result());
        }

        match response.result {
            ResponseResult::Error(error) => {
                state.apply_error(&error);
                if error.code == ErrorCode::StaleFrame {
                    Ok(stale_frame_result(
                        "the desktop was superseded outside this MCP session; call computer_observe",
                    ))
                } else {
                    Ok(structured_error(&error))
                }
            }
            ResponseResult::Ok(DaemonResponse::Act(outcome)) if outcome.image_expired => {
                state.clear();
                Ok(structured_result(McpActOutcome::from_protocol(
                    outcome, None,
                )))
            }
            ResponseResult::Ok(DaemonResponse::Act(outcome)) => {
                let Some(observation) = outcome.observation.as_ref() else {
                    state.clear();
                    return Ok(CallToolResult::structured_error(json!({
                        "code": "internal",
                        "message": "action result omitted its observation; call computer_observe and inspect the desktop before continuing; do not repeat completed actions",
                        "executed": outcome.executed,
                    })));
                };
                let binding = match state
                    .bind_action_observation(&request_id, observation.frame_id.clone())
                {
                    Ok(binding) => binding,
                    Err(error) => {
                        state.clear();
                        return Ok(structured_error(&error));
                    }
                };
                let image_path = observation.image_path.clone();
                let projected = McpObservation::from_protocol(observation, binding.frame);
                let result = image_result(
                    McpActOutcome::from_protocol(outcome, Some(projected)),
                    image_path,
                )
                .await?;
                if context.ct.is_cancelled() || result.is_error == Some(true) {
                    state.clear();
                    if context.ct.is_cancelled() {
                        return Ok(cancelled_result());
                    }
                } else {
                    state.commit(binding);
                    if !is_retry && let Some(current) = &state.current {
                        self.waits.cancel_if_superseded(&current.internal_id).await;
                    }
                }
                Ok(result)
            }
            ResponseResult::Ok(_) => {
                state.clear();
                Ok(unexpected_response("act"))
            }
        }
    }

    async fn request_daemon(
        &self,
        connection: &Connection,
        request_id: String,
        request: DaemonRequest,
    ) -> Result<ResponseEnvelope, CallToolResult> {
        let is_action = matches!(&request, DaemonRequest::Act(_));
        let envelope = RequestEnvelope {
            request_id,
            request,
        };
        let result = tokio::select! {
            biased;
            () = connection.cancelled.cancelled() => return Err(cancelled_result()),
            result = client::request(&connection.socket, &envelope) => result,
        };
        match result {
            Ok(response) => Ok(response),
            Err(error) if is_action => Err(connection_error(
                &connection.target,
                &anyhow::anyhow!(
                    "action outcome is unknown; call computer_observe and inspect the desktop before issuing more actions: {error:#}"
                ),
            )),
            Err(error) => Err(connection_error(&connection.target, &error)),
        }
    }

    async fn start_async_wait(
        &self,
        request: WaitRequest,
        baseline: FrameBinding,
        connection: &Connection,
        context: &RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let mut state = self.state.lock().await;
        if context.ct.is_cancelled() {
            return Ok(cancelled_result());
        }
        if let Err(error) = state.ensure_current(&baseline) {
            return Ok(structured_error(&error));
        }
        let started = match self.waits.start(
            connection.socket.clone(),
            request,
            baseline.clone(),
            Arc::clone(&self.state),
        ) {
            Ok(started) => started,
            Err(error) => return Ok(structured_error(&error)),
        };
        if context.ct.is_cancelled() {
            self.waits.cancel().await;
            state.clear_if_current(&baseline);
            return Ok(cancelled_result());
        }
        Ok(structured_result(McpWaitReply::Started(started)))
    }

    async fn request_wait_daemon(
        &self,
        connection: &Connection,
        request: WaitRequest,
        context: &RequestContext<RoleServer>,
    ) -> Result<ResponseEnvelope, CallToolResult> {
        let request = self.request_daemon(
            connection,
            Uuid::new_v4().to_string(),
            DaemonRequest::Wait(request),
        );
        tokio::pin!(request);
        let started = tokio::time::Instant::now();
        let progress_token = context.meta.get_progress_token();
        let mut progress =
            tokio::time::interval_at(started + WAIT_PROGRESS_INTERVAL, WAIT_PROGRESS_INTERVAL);
        progress.set_missed_tick_behavior(MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                biased;
                () = context.ct.cancelled() => return Err(cancelled_result()),
                response = &mut request => return response,
                _ = progress.tick(), if progress_token.is_some() => {
                    let elapsed = started.elapsed().as_secs_f64();
                    let notification = ProgressNotificationParam::new(
                        progress_token.clone().expect("guarded progress token"),
                        elapsed,
                    )
                    .with_message("Waiting for monitored screen activity");
                    let _ = context.peer.notify_progress(notification).await;
                }
            }
        }
    }

    async fn finish_wait(
        &self,
        outcome: WaitOutcome,
        baseline: FrameBinding,
        context: &RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        match outcome {
            WaitOutcome::Timeout { elapsed_ms } => {
                let state = self.state.lock().await;
                if let Err(error) = state.ensure_current(&baseline) {
                    return Ok(structured_error(&error));
                }
                Ok(structured_result(McpWaitReply::Finished(McpWaitOutcome {
                    status: McpWaitStatus::Timeout,
                    elapsed_ms,
                    frame: baseline.frame,
                    settled: true,
                    activity_bbox: None,
                })))
            }
            WaitOutcome::Changed {
                elapsed_ms,
                activity_bbox,
                observation,
            } => {
                let mut state = self.state.lock().await;
                let binding =
                    match state.bind_wait_observation(&baseline, observation.frame_id.clone()) {
                        Ok(binding) => binding,
                        Err(error) => return Ok(structured_error(&error)),
                    };
                let result = image_result(
                    McpWaitReply::Finished(McpWaitOutcome {
                        status: McpWaitStatus::Changed,
                        elapsed_ms,
                        frame: binding.frame,
                        settled: observation.settled,
                        activity_bbox: Some(activity_bbox),
                    }),
                    observation.image_path,
                )
                .await?;
                if context.ct.is_cancelled() || result.is_error == Some(true) {
                    state.clear_if_current(&baseline);
                    if context.ct.is_cancelled() {
                        return Ok(cancelled_result());
                    }
                } else {
                    state.commit(binding);
                }
                Ok(result)
            }
        }
    }

    fn action_request_id(&self, request_id: &rmcp::model::RequestId, generation: Uuid) -> String {
        let typed_id = match request_id {
            rmcp::model::RequestId::Number(value) => format!("n:{value}"),
            rmcp::model::RequestId::String(value) => format!("s:{value}"),
        };
        format!("mcp:{}:{generation}:{typed_id}", self.session_id)
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for ComputerUseMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(
                Implementation::new("cu", env!("CARGO_PKG_VERSION"))
                    .with_title("cu computer use")
                    .with_description("Frame-grounded control of a local Linux desktop"),
            )
            .with_instructions(MCP_INSTRUCTIONS)
    }

    async fn on_initialized(&self, _context: NotificationContext<RoleServer>) {
        eprintln!("computer-use MCP initialized");
    }
}

pub async fn serve() -> anyhow::Result<()> {
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let server = ComputerUseMcp::new();
    let owner = server.waits.owner();
    let desktop = server.desktop.clone();
    let stopped = server.shutdown.clone();
    let shutdown = async {
        tokio::select! {
            result = tokio::signal::ctrl_c() => result.map_err(anyhow::Error::from),
            result = terminate.recv() => result.ok_or_else(|| anyhow::anyhow!("SIGTERM listener closed")),
            () = stopped.cancelled() => Ok(()),
        }
    };
    tokio::pin!(shutdown);
    let (stdin, stdout) = stdio();
    let transport = transport::WaitTransport::new(
        AsyncRwTransport::new_server(stdin, stdout),
        server.waits.clone(),
        desktop.clone(),
        stopped.clone(),
    );
    let result = async {
        let running = tokio::select! {
            result = server.serve(transport) => result?,
            signal = &mut shutdown => return signal,
        };
        let cancellation = running.cancellation_token();
        let waiting = running.waiting();
        tokio::pin!(waiting);
        tokio::select! {
            result = &mut waiting => result.map(|_| ()).map_err(anyhow::Error::from),
            signal = &mut shutdown => {
                stopped.cancel();
                desktop.stop();
                owner.shutdown().await;
                cancellation.cancel();
                let _ = tokio::time::timeout(Duration::from_secs(2), &mut waiting).await;
                signal
            }
        }
    }
    .await;
    stopped.cancel();
    owner.shutdown().await;
    desktop.shutdown().await;
    result
}

fn connected(state: &McpSessionState) -> Result<Connection, CallToolResult> {
    state.connection.clone().ok_or_else(|| {
        CallToolResult::structured_error(json!({
            "code": "not_connected",
            "message": "call computer_connect before using the desktop; omit name for @default, or set name to @private or an existing instance name",
        }))
    })
}

fn connection_error(target: &ConnectionTarget, error: &anyhow::Error) -> CallToolResult {
    let message = match target {
        ConnectionTarget::Private => format!(
            "private desktop unavailable: {error:#}; call computer_connect with name @private to retry"
        ),
        ConnectionTarget::Default => format!("default daemon unavailable: {error:#}"),
        ConnectionTarget::Named(name) => {
            format!("named instance {name} unavailable: {error:#}")
        }
    };
    CallToolResult::structured_error(json!({ "code": "daemon_unavailable", "message": message }))
}

async fn fetch_profile(socket: &Path) -> anyhow::Result<Option<String>> {
    let response = client::request(
        socket,
        &RequestEnvelope {
            request_id: format!("mcp-profile:{}", Uuid::new_v4()),
            request: DaemonRequest::Profile,
        },
    )
    .await?;
    match response.result {
        ResponseResult::Ok(DaemonResponse::Profile(profile)) => Ok(profile),
        ResponseResult::Error(error) => Err(anyhow::anyhow!(error)),
        ResponseResult::Ok(_) => {
            anyhow::bail!("daemon returned the wrong response to profile request")
        }
    }
}

fn cancelled_connect_result() -> CallToolResult {
    CallToolResult::structured_error(json!({
        "code": "cancelled",
        "message": "the connection attempt was cancelled; call computer_connect before continuing",
    }))
}

fn cancelled_result() -> CallToolResult {
    CallToolResult::structured_error(json!({
        "code": "cancelled",
        "message": "the computer operation was cancelled; call computer_observe and inspect the desktop before continuing",
    }))
}

fn stale_frame_result(message: &str) -> CallToolResult {
    structured_error(&CuError::new(ErrorCode::StaleFrame, message))
}

fn unexpected_response(operation: &str) -> CallToolResult {
    CallToolResult::structured_error(json!({
        "code": "internal",
        "message": format!("daemon returned the wrong response to computer_{operation}; call computer_observe and inspect the desktop before continuing"),
    }))
}

fn with_recovery(mut error: CuError) -> CuError {
    let hint = match error.code {
        ErrorCode::Indeterminate | ErrorCode::InputFailed | ErrorCode::PartialExecution => {
            "call computer_observe and inspect the desktop before issuing more actions; do not repeat completed actions"
        }
        ErrorCode::CaptureFailed | ErrorCode::ViewportChanged | ErrorCode::Cancelled => {
            "call computer_observe before continuing"
        }
        ErrorCode::Busy => "wait for the active wait to finish before starting another",
        _ => return error,
    };
    error.message.push_str("; ");
    error.message.push_str(hint);
    error
}

fn structured_error(error: &CuError) -> CallToolResult {
    let error = with_recovery(error.clone());
    match serde_json::to_value(error) {
        Ok(value) => CallToolResult::structured_error(value),
        Err(error) => CallToolResult::error(vec![ContentBlock::text(error.to_string())]),
    }
}

fn structured_result(metadata: impl Serialize) -> CallToolResult {
    match serde_json::to_value(metadata) {
        Ok(value) => CallToolResult::structured(value),
        Err(error) => CallToolResult::structured_error(json!({
            "code": "internal",
            "message": format!("failed to encode structured result: {error}"),
        })),
    }
}

async fn image_result(
    metadata: impl Serialize,
    image_path: String,
) -> Result<CallToolResult, McpError> {
    let metadata = match serde_json::to_value(metadata) {
        Ok(metadata) => metadata,
        Err(error) => {
            return Ok(CallToolResult::structured_error(json!({
                "code": "internal",
                "message": format!("failed to encode structured result: {error}"),
            })));
        }
    };
    let image = match tokio::fs::read(&image_path).await {
        Ok(image) => image,
        Err(error) => {
            let mut failure = json!({
                "code": "image_unavailable",
                "message": format!("failed to read captured PNG: {error}; call computer_observe and inspect the desktop before continuing"),
            });
            if let Some(executed) = metadata.get("executed") {
                failure["executed"] = executed.clone();
                if let Some(action_error) = metadata.get("action_error") {
                    failure["action_error"] = action_error.clone();
                }
                failure["message"] = json!(format!(
                    "failed to read the post-action PNG: {error}; {executed} actions are recorded as executed. Call computer_observe and inspect the desktop before continuing; do not repeat completed actions"
                ));
            }
            return Ok(CallToolResult::structured_error(failure));
        }
    };
    let mut result = CallToolResult::structured(metadata);
    result
        .content
        .push(ContentBlock::image(STANDARD.encode(image), "image/png"));
    Ok(result)
}

#[cfg(test)]
mod tests {
    use serde_json::Value;
    use tempfile::TempDir;

    use super::*;

    fn request(frame: u64, text: &str) -> McpActRequest {
        McpActRequest {
            frame,
            actions: vec![Action::Type {
                text: text.to_owned(),
            }],
            settle: SettlePolicy::default(),
        }
    }

    fn tool(server: &ComputerUseMcp, name: &str) -> rmcp::model::Tool {
        server
            .tool_router
            .list_all()
            .into_iter()
            .find(|tool| tool.name == name)
            .unwrap_or_else(|| panic!("missing tool {name}"))
    }

    #[test]
    fn exposes_only_the_agent_loop_tools() {
        let server = ComputerUseMcp::new();
        let mut names = server
            .tool_router
            .list_all()
            .into_iter()
            .map(|tool| tool.name.into_owned())
            .collect::<Vec<_>>();
        names.sort();

        assert_eq!(
            names,
            [
                "computer_act",
                "computer_connect",
                "computer_observe",
                "computer_wait"
            ]
        );
    }

    #[test]
    fn identifies_itself_and_explains_the_complete_agent_loop() {
        let server = ComputerUseMcp::new();
        let info = server.get_info();

        assert_eq!(info.server_info.name, "cu");
        assert_eq!(info.server_info.version, env!("CARGO_PKG_VERSION"));
        let instructions = info.instructions.unwrap();
        for required in [
            "computer_observe",
            "computer_wait",
            "latest returned frame number",
            "[0,width)",
            "authorization policy",
        ] {
            assert!(
                instructions.contains(required),
                "MCP instructions omit {required:?}"
            );
        }
        assert!(!instructions.contains("frame_id"));
    }

    #[test]
    fn initialization_requires_connection_and_connect_exposes_one_optional_name() {
        let server = ComputerUseMcp::new();
        let instructions = server.get_info().instructions.unwrap();
        assert!(instructions.contains("computer_connect"));
        assert!(!instructions.contains("Desktop profile:"));
        let connect = tool(&server, "computer_connect");
        let schema = serde_json::to_value(connect.input_schema).unwrap();
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["additionalProperties"], false);
        assert_eq!(schema["properties"].as_object().unwrap().len(), 1);
        assert!(
            schema
                .get("required")
                .is_none_or(|required| required == &json!([]))
        );
        for keyword in ["oneOf", "anyOf", "allOf"] {
            assert!(schema.get(keyword).is_none());
        }
        let name = &schema["properties"]["name"];
        assert_eq!(name["type"], "string");
        assert_eq!(name["default"], "@default");
        assert_eq!(name["minLength"], 1);
        assert_eq!(name["maxLength"], 64);
        assert_eq!(name["pattern"], "^(@default|@private|[A-Za-z0-9_.-]+)$");
        let description = name["description"].as_str().unwrap();
        for meaning in [
            "@default",
            "when omitted",
            "@private",
            "session-local",
            "exits",
            "instances",
        ] {
            assert!(
                description.contains(meaning),
                "name description omits {meaning}"
            );
        }
    }

    #[test]
    fn connect_defaults_and_parses_explicit_selectors_and_literal_names() {
        for (value, expected) in [
            (json!({}), ConnectionTarget::Default),
            (json!({"name": "@default"}), ConnectionTarget::Default),
            (json!({"name": "@private"}), ConnectionTarget::Private),
        ] {
            let request = serde_json::from_value::<McpConnectRequest>(value).unwrap();
            assert_eq!(request.name.parse::<ConnectionTarget>().unwrap(), expected);
        }
        for expected in [
            "work".to_owned(),
            "default".to_owned(),
            "private".to_owned(),
            "x11-99.A_1".to_owned(),
            "a".repeat(64),
        ] {
            let request =
                serde_json::from_value::<McpConnectRequest>(json!({"name": expected})).unwrap();
            let ConnectionTarget::Named(name) = request.name.parse().unwrap() else {
                panic!("expected named target")
            };
            assert_eq!(name.to_string(), expected);
        }
    }

    #[test]
    fn connect_rejects_malformed_arguments_legacy_shapes_and_extra_fields() {
        for value in [
            json!(null),
            json!({"name": null}),
            json!({"name": 42}),
            json!({"name": []}),
            json!({"name": {}}),
            json!({"extra": true}),
            json!({"name": "@private", "extra": true}),
            json!({"type": null}),
            json!({"type": "work"}),
            json!({"type": "PRIVATE"}),
            json!({"instance": "default"}),
            json!({"type": "private"}),
            json!({"type": "default"}),
            json!({"type": "named"}),
            json!({"type": "named", "name": null}),
            json!({"type": "private", "name": "work"}),
            json!({"type": "private", "name": null}),
            json!({"type": "default", "name": "work"}),
            json!({"type": "default", "name": null}),
            json!({"type": "private", "extra": true}),
            json!({"type": "default", "instance": "work"}),
            json!({"type": "named", "name": "work", "extra": true}),
            json!({"type": "named", "name": "work"}),
        ] {
            assert!(
                serde_json::from_value::<McpConnectRequest>(value.clone()).is_err(),
                "accepted {value}"
            );
        }
    }

    #[test]
    fn connection_target_rejects_invalid_names_and_unknown_selectors() {
        for name in [
            "",
            " ",
            ".",
            "..",
            "../escape",
            "has/slash",
            "空",
            "@unknown",
            "@DEFAULT",
            &"a".repeat(65),
        ] {
            assert!(name.parse::<ConnectionTarget>().is_err(), "accepted {name}");
        }
    }

    #[test]
    fn publishes_described_observe_and_action_schemas() {
        let server = ComputerUseMcp::new();
        let observe = tool(&server, "computer_observe");
        let act = tool(&server, "computer_act");
        let wait = tool(&server, "computer_wait");

        let published = serde_json::to_string(&[&observe, &act, &wait]).unwrap();
        assert!(!published.contains("frame_id"));
        assert!(!published.contains("expected_frame_id"));

        let observe_annotations = observe.annotations.unwrap();
        assert_eq!(observe_annotations.read_only_hint, Some(true));
        assert_eq!(observe_annotations.destructive_hint, Some(false));
        assert!(observe.output_schema.is_some());
        assert!(
            observe.input_schema["properties"]["settle"]["description"]
                .as_str()
                .is_some_and(|description| description.contains("150 ms"))
        );

        let act_annotations = act.annotations.unwrap();
        assert_eq!(act_annotations.read_only_hint, Some(false));
        assert_eq!(act_annotations.destructive_hint, Some(true));
        assert_eq!(act_annotations.idempotent_hint, Some(false));
        assert!(act.output_schema.is_some());

        let schema = act.input_schema;
        assert_eq!(schema["properties"]["actions"]["minItems"], 1);
        assert_eq!(schema["properties"]["actions"]["maxItems"], 16);
        assert_eq!(schema["properties"]["frame"]["type"], "integer");
        assert_eq!(schema["properties"]["frame"]["minimum"], 1);
        assert!(schema["properties"].get("expected_frame_id").is_none());
        assert!(
            schema["required"]
                .as_array()
                .is_some_and(|required| required.iter().any(|field| field == "frame"))
        );
        assert_eq!(
            schema["$defs"]["SettlePolicy"]["properties"]["quiet_ms"]["minimum"],
            1
        );
        assert_eq!(
            schema["$defs"]["SettlePolicy"]["properties"]["quiet_ms"]["maximum"],
            5000
        );
        assert_eq!(
            schema["$defs"]["SettlePolicy"]["properties"]["timeout_ms"]["maximum"],
            30000
        );

        let actions = schema["$defs"]["Action"]["oneOf"]
            .as_array()
            .expect("action union");
        assert_eq!(actions.len(), 7);
        let drag = actions
            .iter()
            .find(|action| action["properties"]["type"]["const"] == "drag")
            .expect("drag action");
        assert_eq!(drag["properties"]["path"]["minItems"], 2);
        assert_eq!(drag["properties"]["path"]["maxItems"], 256);
        assert!(
            drag["description"]
                .as_str()
                .is_some_and(|description| description.contains("left mouse button"))
        );

        let observe_output = observe.output_schema.unwrap();
        assert_eq!(observe_output["properties"]["frame"]["type"], "integer");
        assert_eq!(observe_output["properties"]["frame"]["minimum"], 1);
        for omitted in ["frame_id", "image_path", "target", "coordinate_space"] {
            assert!(
                observe_output["properties"].get(omitted).is_none(),
                "observe output exposes {omitted}"
            );
        }
        let act_output = act.output_schema.unwrap();
        assert!(
            act_output["properties"]["observation"]["description"]
                .as_str()
                .is_some_and(|description| description.contains("image_expired"))
        );
        assert_eq!(act_output["properties"]["image_expired"]["type"], "boolean");
        let error_code = &act_output["$defs"]["McpActionError"]["properties"]["code"];
        assert_eq!(error_code["type"], "string");
        assert!(error_code.get("enum").is_none());
        assert!(act_output["$defs"].get("ErrorCode").is_none());
        let act_observation_properties = &act_output["$defs"]["McpObservation"]["properties"];
        for omitted in ["frame_id", "image_path", "target", "coordinate_space"] {
            assert!(
                act_observation_properties.get(omitted).is_none(),
                "act output exposes {omitted}"
            );
        }
        let action_error_executed =
            &act_output["$defs"]["McpActionError"]["properties"]["executed"];
        assert_eq!(action_error_executed["type"], "integer");
        assert!(
            act_output["$defs"]["McpActionError"]["required"]
                .as_array()
                .is_some_and(|required| required.iter().any(|field| field == "executed"))
        );
    }

    #[test]
    fn publishes_the_bounded_wait_schema_without_internal_frame_ids() {
        let server = ComputerUseMcp::new();
        let wait = tool(&server, "computer_wait");
        let wait_annotations = wait.annotations.unwrap();
        assert_eq!(wait_annotations.read_only_hint, Some(true));
        assert_eq!(wait_annotations.destructive_hint, Some(false));
        assert_eq!(wait_annotations.idempotent_hint, Some(false));

        let wait_schema = wait.input_schema;
        assert_eq!(wait_schema["properties"]["async"]["type"], "boolean");
        assert_eq!(wait_schema["properties"]["async"]["default"], false);
        assert!(
            !wait_schema["required"]
                .as_array()
                .unwrap()
                .iter()
                .any(|field| field == "async")
        );
        assert_eq!(wait_schema["properties"]["frame"]["minimum"], 1);
        assert_eq!(wait_schema["properties"]["include_rects"]["minItems"], 1);
        assert_eq!(wait_schema["properties"]["include_rects"]["maxItems"], 8);
        assert_eq!(wait_schema["properties"]["exclude_rects"]["maxItems"], 8);
        for dimension in ["width", "height"] {
            assert_eq!(
                wait_schema["$defs"]["Rect"]["properties"][dimension]["minimum"],
                1
            );
        }
        assert_eq!(
            wait_schema["properties"]["timeout_ms"]["maximum"],
            3_600_000
        );
        assert!(wait_schema["properties"].get("coalesce_ms").is_none());
        assert_eq!(wait_schema["properties"]["quiet_ms"]["default"], 2_000);
        assert_eq!(wait_schema["properties"]["quiet_ms"]["maximum"], 60_000);
        assert!(wait_schema["properties"].get("settle_max_ms").is_none());

        let wait_output = wait.output_schema.unwrap();
        assert_eq!(wait_output["type"], "object");
        assert_eq!(wait_output["anyOf"].as_array().unwrap().len(), 2);
        let wait_output = &wait_output["$defs"]["McpWaitOutcome"];
        assert_eq!(wait_output["properties"]["elapsed_ms"]["type"], "integer");
        assert_eq!(wait_output["properties"]["frame"]["minimum"], 1);
        assert!(wait_output["properties"].get("frame_id").is_none());
        for omitted in ["observation", "width", "height", "image_path", "target"] {
            assert!(wait_output["properties"].get(omitted).is_none());
        }
    }

    #[test]
    fn normal_action_result_contains_every_schema_required_field() {
        let server = ComputerUseMcp::new();
        let act_output = tool(&server, "computer_act").output_schema.unwrap();
        let normal = serde_json::to_value(McpActOutcome {
            status: ActStatus::Ok,
            executed: 1,
            action_error: None,
            image_expired: false,
            observation: Some(McpObservation {
                frame: 1,
                width: 100,
                height: 80,
                settled: true,
            }),
            message: None,
        })
        .unwrap();
        for required in act_output["required"].as_array().unwrap() {
            let required = required.as_str().unwrap();
            assert!(
                normal.get(required).is_some(),
                "normal action result omits schema-required field {required}"
            );
        }
    }

    #[tokio::test]
    async fn missing_daemon_returns_a_start_command_to_the_agent() {
        let directory = TempDir::new().unwrap();
        let server = ComputerUseMcp::new();
        let connection = Connection {
            socket: directory.path().join("missing.sock"),
            target: ConnectionTarget::Default,
            generation: Uuid::new_v4(),
            cancelled: CancellationToken::new(),
        };

        let Err(result) = server
            .request_daemon(
                &connection,
                "missing-daemon-test".to_owned(),
                DaemonRequest::Observe(ObserveRequest::default()),
            )
            .await
        else {
            panic!("missing daemon unexpectedly responded");
        };

        assert_eq!(result.is_error, Some(true));
        assert_eq!(
            result.structured_content.as_ref().unwrap()["code"],
            "daemon_unavailable"
        );
        let message = result.content[0].as_text().unwrap().text.as_str();
        assert!(message.contains("start it separately, then retry"));
        assert!(message.contains("`cu daemon`"));
        assert!(message.contains("`cu daemon --help`"));
    }

    #[tokio::test]
    async fn successful_observation_returns_matching_structured_data_and_png() {
        let directory = TempDir::new().unwrap();
        let image_path = directory.path().join("frame.png");
        tokio::fs::write(&image_path, [1, 2, 3]).await.unwrap();
        let observation = cu_protocol::Observation {
            frame_id: "f_test_1".to_owned(),
            target: "test:screen".to_owned(),
            width: 100,
            height: 80,
            coordinate_space: cu_protocol::CoordinateSpace::FramePixels,
            settled: true,
            image_path: image_path.to_string_lossy().into_owned(),
        };
        let expected = json!({
            "frame": 1,
            "width": 100,
            "height": 80,
            "settled": true,
        });

        let result = image_result(
            McpObservation::from_protocol(&observation, 1),
            observation.image_path,
        )
        .await
        .unwrap();

        assert_eq!(result.is_error, Some(false));
        assert_eq!(result.structured_content, Some(expected.clone()));
        assert_eq!(result.content.len(), 2);
        let text = result.content[0].as_text().unwrap();
        assert!(!text.text.contains("image_path"));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&text.text).unwrap(),
            expected
        );
        assert!(result.content[1].as_image().is_some());
    }

    #[tokio::test]
    async fn changed_wait_returns_compact_metadata_and_native_png() {
        let directory = TempDir::new().unwrap();
        let image_path = directory.path().join("wait.png");
        tokio::fs::write(&image_path, [1, 2, 3]).await.unwrap();
        let result = image_result(
            McpWaitOutcome {
                status: McpWaitStatus::Changed,
                elapsed_ms: 32_784,
                frame: 2,
                settled: true,
                activity_bbox: Some(Rect {
                    x: 120,
                    y: 240,
                    width: 600,
                    height: 310,
                }),
            },
            image_path.to_string_lossy().into_owned(),
        )
        .await
        .unwrap();

        assert_eq!(result.is_error, Some(false));
        let structured = result.structured_content.as_ref().unwrap();
        assert_eq!(structured["status"], "changed");
        assert_eq!(structured["elapsed_ms"], 32_784);
        assert_eq!(structured["frame"], 2);
        assert_eq!(structured["settled"], true);
        assert!(structured.get("observation").is_none());
        assert_eq!(result.content.len(), 2);
        assert!(result.content[1].as_image().is_some());
    }

    #[test]
    fn timed_out_wait_preserves_its_frame_without_an_image() {
        let result = structured_result(McpWaitOutcome {
            status: McpWaitStatus::Timeout,
            elapsed_ms: 30_012,
            frame: 7,
            settled: true,
            activity_bbox: None,
        });

        assert_eq!(result.is_error, Some(false));
        let structured = result.structured_content.as_ref().unwrap();
        assert_eq!(structured["status"], "timeout");
        assert_eq!(structured["frame"], 7);
        assert!(structured.get("observation").is_none());
        assert!(
            result
                .content
                .iter()
                .all(|content| content.as_image().is_none())
        );
    }

    #[tokio::test]
    async fn unavailable_image_error_omits_the_private_path() {
        let directory = TempDir::new().unwrap();
        let image_path = directory.path().join("missing-frame.png");
        let private_path = image_path.to_string_lossy().into_owned();

        let result = image_result(json!({"frame": 1}), private_path.clone())
            .await
            .unwrap();

        assert_eq!(result.is_error, Some(true));
        let structured = result.structured_content.as_ref().unwrap();
        assert_eq!(structured["code"], "image_unavailable");
        assert!(
            !structured["message"]
                .as_str()
                .unwrap()
                .contains(&private_path)
        );
    }

    #[tokio::test]
    async fn successful_action_omits_the_nested_image_path_and_returns_png() {
        let directory = TempDir::new().unwrap();
        let image_path = directory.path().join("frame.png");
        tokio::fs::write(&image_path, [1, 2, 3]).await.unwrap();
        let outcome = ActOutcome {
            status: ActStatus::Ok,
            executed: 1,
            action_error: None,
            image_expired: false,
            observation: Some(Observation {
                frame_id: "f_test_2".to_owned(),
                target: "test:screen".to_owned(),
                width: 100,
                height: 80,
                coordinate_space: cu_protocol::CoordinateSpace::FramePixels,
                settled: true,
                image_path: image_path.to_string_lossy().into_owned(),
            }),
        };
        let projected = McpObservation::from_protocol(outcome.observation.as_ref().unwrap(), 2);
        let image_path = outcome.observation.as_ref().unwrap().image_path.clone();

        let result = image_result(
            McpActOutcome::from_protocol(outcome, Some(projected)),
            image_path,
        )
        .await
        .unwrap();

        let structured = result.structured_content.as_ref().unwrap();
        assert_eq!(structured["status"], "ok");
        assert_eq!(structured["executed"], 1);
        assert_eq!(structured["observation"]["frame"], 2);
        assert!(structured.get("message").is_none());
        for omitted in ["frame_id", "image_path", "target", "coordinate_space"] {
            assert!(structured["observation"].get(omitted).is_none());
        }
        assert_eq!(result.content.len(), 2);
        assert!(result.content[1].as_image().is_some());
    }

    #[tokio::test]
    async fn partial_action_returns_a_required_integer_error_count() {
        let directory = TempDir::new().unwrap();
        let image_path = directory.path().join("frame.png");
        tokio::fs::write(&image_path, [1, 2, 3]).await.unwrap();
        let outcome = ActOutcome {
            status: ActStatus::Partial,
            executed: 1,
            action_error: Some(
                CuError::new(ErrorCode::InputFailed, "failed after the first action")
                    .with_executed(1),
            ),
            image_expired: false,
            observation: Some(Observation {
                frame_id: "f_test_partial".to_owned(),
                target: "test:screen".to_owned(),
                width: 100,
                height: 80,
                coordinate_space: cu_protocol::CoordinateSpace::FramePixels,
                settled: true,
                image_path: image_path.to_string_lossy().into_owned(),
            }),
        };
        let projected = McpObservation::from_protocol(outcome.observation.as_ref().unwrap(), 3);
        let image_path = outcome.observation.as_ref().unwrap().image_path.clone();

        let result = image_result(
            McpActOutcome::from_protocol(outcome, Some(projected)),
            image_path,
        )
        .await
        .unwrap();

        let structured = result.structured_content.as_ref().unwrap();
        assert_eq!(structured["status"], "partial");
        assert_eq!(structured["executed"], 1);
        assert_eq!(structured["action_error"]["code"], "input_failed");
        assert_eq!(structured["action_error"]["executed"], 1);
        assert!(structured["action_error"]["executed"].is_number());
        assert_eq!(structured["observation"]["frame"], 3);
        assert!(
            structured["message"]
                .as_str()
                .unwrap()
                .contains("Inspect the returned observation")
        );
        assert!(
            structured["message"]
                .as_str()
                .unwrap()
                .contains("do not repeat")
        );
        assert_eq!(
            serde_json::from_str::<Value>(&result.content[0].as_text().unwrap().text).unwrap(),
            *structured
        );
        assert_eq!(result.content.len(), 2);
        assert!(result.content[1].as_image().is_some());
    }

    #[tokio::test]
    async fn expired_cached_action_is_a_non_error_without_an_image() {
        let result = structured_result(McpActOutcome::from_protocol(
            ActOutcome {
                status: ActStatus::Ok,
                executed: 1,
                action_error: None,
                image_expired: true,
                observation: None,
            },
            None,
        ));

        assert_eq!(result.is_error, Some(false));
        let structured = result.structured_content.as_ref().unwrap();
        assert_eq!(structured["status"], "ok");
        assert_eq!(structured["executed"], 1);
        assert_eq!(structured["image_expired"], true);
        assert!(structured.get("observation").is_none());
        assert!(
            structured["message"]
                .as_str()
                .unwrap()
                .contains("computer_observe")
        );
        assert!(
            structured["message"]
                .as_str()
                .unwrap()
                .contains("do not repeat")
        );
        assert_eq!(
            serde_json::from_str::<Value>(&result.content[0].as_text().unwrap().text).unwrap(),
            *structured
        );
        assert!(
            result
                .content
                .iter()
                .all(|content| content.as_image().is_none())
        );
    }

    #[test]
    fn action_requires_a_current_mcp_frame() {
        let mut state = McpSessionState::default();

        let error = state.prepare_action("request-1", &request(1, "hello"));

        assert_eq!(error.unwrap_err().code, ErrorCode::StaleFrame);
        assert!(state.completed_actions.is_empty());
    }

    #[test]
    fn observation_and_action_results_form_a_compact_sequence() {
        let mut state = McpSessionState::default();
        let first = state
            .bind_observation("internal-frame-1".to_owned())
            .unwrap();
        state.commit(first.clone());

        let protocol = state
            .prepare_action("request-1", &request(first.frame, "hello"))
            .unwrap();
        assert_eq!(first.frame, 1);
        assert_eq!(protocol.expected_frame_id, "internal-frame-1");

        let second = state
            .bind_action_observation("request-1", "internal-frame-2".to_owned())
            .unwrap();
        state.commit(second.clone());
        let next = state
            .prepare_action("request-2", &request(second.frame, "world"))
            .unwrap();

        assert_eq!(second.frame, 2);
        assert_eq!(next.expected_frame_id, "internal-frame-2");
    }

    #[test]
    fn wait_results_contain_every_schema_required_field() {
        let server = ComputerUseMcp::new();
        let wait_output = tool(&server, "computer_wait").output_schema.unwrap();
        let wait_output = &wait_output["$defs"]["McpWaitOutcome"];
        let outcomes = [
            McpWaitOutcome {
                status: McpWaitStatus::Timeout,
                elapsed_ms: 30_000,
                frame: 1,
                settled: true,
                activity_bbox: None,
            },
            McpWaitOutcome {
                status: McpWaitStatus::Changed,
                elapsed_ms: 2_500,
                frame: 2,
                settled: false,
                activity_bbox: Some(Rect {
                    x: 10,
                    y: 20,
                    width: 30,
                    height: 40,
                }),
            },
        ];

        for outcome in outcomes {
            let serialized = serde_json::to_value(outcome).unwrap();
            for required in wait_output["required"].as_array().unwrap() {
                let required = required.as_str().unwrap();
                assert!(
                    serialized.get(required).is_some(),
                    "wait result omits schema-required field {required}"
                );
            }
        }
    }

    #[test]
    fn stale_wait_errors_do_not_invalidate_the_session_frame() {
        assert!(!wait_error_invalidates_frame(ErrorCode::StaleFrame));
        assert!(!wait_error_invalidates_frame(ErrorCode::Busy));
        assert!(!wait_error_invalidates_frame(ErrorCode::InvalidAction));
        assert!(wait_error_invalidates_frame(ErrorCode::ViewportChanged));
        assert!(wait_error_invalidates_frame(ErrorCode::CaptureFailed));
    }

    #[test]
    fn changed_wait_advances_the_frame_only_if_its_baseline_is_current() {
        let mut state = McpSessionState::default();
        let baseline = state
            .bind_observation("internal-frame-1".to_owned())
            .unwrap();
        state.commit(baseline.clone());

        let changed = state
            .bind_wait_observation(&baseline, "internal-frame-2".to_owned())
            .unwrap();
        state.commit(changed.clone());

        assert_eq!(changed.frame, 2);
        assert_eq!(state.current, Some(changed));
        assert_eq!(
            state
                .bind_wait_observation(&baseline, "internal-frame-3".to_owned())
                .unwrap_err()
                .code,
            ErrorCode::StaleFrame
        );
    }

    #[test]
    fn exact_retry_reuses_its_original_input_and_output_frames() {
        let mut state = McpSessionState::default();
        let first = state
            .bind_observation("internal-frame-1".to_owned())
            .unwrap();
        state.commit(first.clone());
        let public_request = request(first.frame, "hello");
        let original = state.prepare_action("request-1", &public_request).unwrap();
        let second = state
            .bind_action_observation("request-1", "internal-frame-2".to_owned())
            .unwrap();
        state.commit(second.clone());

        let replay = state.prepare_action("request-1", &public_request).unwrap();
        let replayed_result = state
            .bind_action_observation("request-1", "internal-frame-2".to_owned())
            .unwrap();

        assert_eq!(replay, original);
        assert_eq!(replayed_result, second);
        assert_eq!(state.latest_frame, 2);
    }

    #[test]
    fn request_id_reuse_with_different_input_is_rejected() {
        let mut state = McpSessionState::default();
        let first = state
            .bind_observation("internal-frame-1".to_owned())
            .unwrap();
        state.commit(first.clone());
        state
            .prepare_action("request-1", &request(first.frame, "hello"))
            .unwrap();

        let error = state
            .prepare_action("request-1", &request(first.frame, "different"))
            .unwrap_err();

        assert_eq!(error.code, ErrorCode::ProtocolError);
    }

    #[test]
    fn replayed_old_result_does_not_replace_a_newer_current_frame() {
        let mut state = McpSessionState::default();
        let first = state
            .bind_observation("internal-frame-1".to_owned())
            .unwrap();
        state.commit(first.clone());
        state
            .prepare_action("request-1", &request(first.frame, "first"))
            .unwrap();
        let second = state
            .bind_action_observation("request-1", "internal-frame-2".to_owned())
            .unwrap();
        state.commit(second.clone());
        let third = state
            .bind_observation("internal-frame-3".to_owned())
            .unwrap();
        state.commit(third.clone());

        state.commit(second);

        assert_eq!(state.current, Some(third));
    }

    #[test]
    fn uncertain_errors_clear_grounding_but_validation_errors_preserve_it() {
        let mut state = McpSessionState::default();
        let first = state
            .bind_observation("internal-frame-1".to_owned())
            .unwrap();
        state.commit(first.clone());

        state.apply_error(&CuError::new(ErrorCode::OutOfBounds, "outside frame"));
        assert_eq!(state.current, Some(first.clone()));

        state.apply_error(&CuError::new(
            ErrorCode::InputFailed,
            "input may have changed",
        ));
        assert!(state.current.is_none());
    }

    #[test]
    fn action_request_memo_is_bounded() {
        let mut state = McpSessionState::default();
        let first = state
            .bind_observation("internal-frame-1".to_owned())
            .unwrap();
        state.commit(first.clone());

        for index in 0..=MAX_MCP_CACHED_ACTIONS {
            state
                .prepare_action(
                    &format!("request-{index}"),
                    &request(first.frame, &index.to_string()),
                )
                .unwrap();
        }

        assert_eq!(state.completed_actions.len(), MAX_MCP_CACHED_ACTIONS);
        assert!(!state.completed_actions.contains_key("request-0"));
        assert!(
            state
                .completed_actions
                .contains_key(&format!("request-{MAX_MCP_CACHED_ACTIONS}"))
        );
    }

    #[test]
    fn numeric_and_string_request_ids_have_distinct_daemon_keys() {
        let server = ComputerUseMcp::new();

        let numeric = server.action_request_id(&rmcp::model::RequestId::Number(7), Uuid::nil());
        let string =
            server.action_request_id(&rmcp::model::RequestId::String("7".into()), Uuid::nil());

        assert_ne!(numeric, string);
        assert!(numeric.ends_with(":n:7"));
        assert!(string.ends_with(":s:7"));
    }

    #[test]
    fn stale_frame_error_does_not_expose_internal_ids() {
        let result = stale_frame_result("call computer_observe");
        let text = result.content[0].as_text().unwrap().text.as_str();

        assert!(!text.contains("internal-frame"));
        assert!(!text.contains("f_"));
        assert_eq!(
            result.structured_content.as_ref().unwrap()["code"],
            "stale_frame"
        );
    }
}
