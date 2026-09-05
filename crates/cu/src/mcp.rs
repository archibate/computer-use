use std::{
    collections::{HashMap, VecDeque},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use cu_protocol::{
    ActOutcome, ActRequest, ActStatus, Action, CuError, DEFAULT_WAIT_COALESCE_MS,
    DEFAULT_WAIT_QUIET_MS, DEFAULT_WAIT_SETTLE_MS, DEFAULT_WAIT_TIMEOUT_MS, DaemonRequest,
    DaemonResponse, ErrorCode, Observation, ObserveRequest, Rect, RequestEnvelope,
    ResponseEnvelope, ResponseResult, SettlePolicy, WaitOutcome, WaitRequest, WaitStatus,
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
    transport::stdio,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::{sync::Mutex, time::MissedTickBehavior};
use uuid::Uuid;

use crate::client;

const MCP_INSTRUCTIONS: &str = "Use computer_observe before the first action and whenever the current screenshot is unknown. Pass the latest returned frame number to computer_act.frame or computer_wait.frame; x and y are integer pixels in [0,width) and [0,height). computer_act affects the live desktop and returns a fresh observation. computer_wait pauses without repeated observation calls: an unchanged timeout returns no image and preserves its frame, while detected activity returns a fresh observation and PNG. Re-arm a timed-out wait with the same frame or a changed wait with its returned frame. Repeated waits with unexpectedly small elapsed_ms usually indicate that an included region contains irrelevant motion; narrow include_rects or exclude carets and animations. Batch actions only when no intermediate inspection is needed. After stale_frame, cancellation, viewport_changed, or image_expired, observe again. After partial execution, inspect the returned observation before continuing. Apply your authorization policy before consequential UI actions.";
const PROFILE_FETCH_TIMEOUT: Duration = Duration::from_secs(5);
const WAIT_PROGRESS_INTERVAL: Duration = Duration::from_secs(5);
const MAX_MCP_CACHED_ACTIONS: usize = 64;

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
    /// Maximum time to detect the first change; defaults to 30000 ms.
    #[serde(default = "default_wait_timeout_ms")]
    #[schemars(range(min = 1, max = 600_000))]
    timeout_ms: u64,
    /// Minimum batching window after the first change; defaults to 2000 ms.
    #[serde(default = "default_wait_coalesce_ms")]
    #[schemars(range(min = 0, max = 60_000))]
    coalesce_ms: u64,
    /// Required continuous stability after activity; defaults to 1000 ms.
    #[serde(default = "default_wait_quiet_ms")]
    #[schemars(range(min = 1, max = 10_000))]
    quiet_ms: u64,
    /// Hard post-change limit; defaults to 15000 ms.
    #[serde(default = "default_wait_settle_ms")]
    #[schemars(range(min = 1, max = 120_000))]
    settle_max_ms: u64,
}

const fn default_wait_timeout_ms() -> u64 {
    DEFAULT_WAIT_TIMEOUT_MS
}

const fn default_wait_coalesce_ms() -> u64 {
    DEFAULT_WAIT_COALESCE_MS
}

const fn default_wait_quiet_ms() -> u64 {
    DEFAULT_WAIT_QUIET_MS
}

const fn default_wait_settle_ms() -> u64 {
    DEFAULT_WAIT_SETTLE_MS
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
struct McpWaitOutcome {
    /// Whether monitored activity was observed or the unchanged timeout elapsed.
    status: WaitStatus,
    /// Monotonic time through the terminal screen sample; excludes result encoding and transport.
    elapsed_ms: u64,
    /// Current session-local frame: unchanged on timeout and new after activity.
    #[schemars(range(min = 1))]
    frame: u64,
    /// Bounding box covering monitored activity since the first change.
    #[serde(skip_serializing_if = "Option::is_none")]
    activity_bbox: Option<Rect>,
    /// Fresh frame metadata after activity; absent on timeout.
    #[serde(skip_serializing_if = "Option::is_none")]
    observation: Option<McpObservation>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct McpActionError {
    /// Stable error category.
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
    /// True only when a cached replay outlived its screenshot; observe again without repeating it.
    #[serde(default, skip_serializing_if = "is_false")]
    image_expired: bool,
    /// Fresh post-action or post-failure frame, absent only when `image_expired` is true.
    #[serde(skip_serializing_if = "Option::is_none")]
    observation: Option<McpObservation>,
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
    latest_frame: u64,
    current: Option<FrameBinding>,
    completed_actions: HashMap<String, CachedMcpAction>,
    cache_order: VecDeque<String>,
}

impl McpSessionState {
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
            | ErrorCode::InvalidAction
            | ErrorCode::OutOfBounds
            | ErrorCode::UnsupportedInput
            | ErrorCode::ProtocolError
    )
}

#[derive(Clone)]
pub struct ComputerUseMcp {
    socket: PathBuf,
    instructions: String,
    session_id: Uuid,
    state: Arc<Mutex<McpSessionState>>,
    tool_router: rmcp::handler::server::tool::ToolRouter<Self>,
}

#[tool_router(router = tool_router)]
impl ComputerUseMcp {
    pub fn new(socket: PathBuf, profile: Option<&str>) -> Self {
        Self {
            socket,
            instructions: compose_instructions(profile),
            session_id: Uuid::new_v4(),
            state: Arc::new(Mutex::new(McpSessionState::default())),
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        description = "Capture the current desktop after optional visual settling. Returns a session-local frame number, image dimensions, settling status, and a PNG. Call before the first action, after stale_frame, or whenever UI state is uncertain.",
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
        if context.ct.is_cancelled() {
            return Ok(cancelled_result());
        }
        let response = match self
            .request_daemon(Uuid::new_v4().to_string(), DaemonRequest::Observe(request))
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
        description = "Wait for exact RGB pixel activity in the union of include_rects minus exclude_rects, relative to the supplied current frame. After the first change, coalesce activity and require a quiet interval, bounded by settle_max_ms. A changed result returns a fresh frame and PNG; an unchanged timeout returns no image and preserves the supplied frame. Repeated short waits suggest narrowing include_rects or excluding carets and animations.",
        output_schema = rmcp::handler::server::tool::schema_for_output::<McpWaitOutcome>(),
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
        let baseline = {
            let state = self.state.lock().await;
            match state.resolve_frame(request.frame) {
                Ok(binding) => binding,
                Err(error) => return Ok(structured_error(&error)),
            }
        };
        let protocol_request = WaitRequest {
            last_frame_id: baseline.internal_id.clone(),
            include_rects: request.include_rects,
            exclude_rects: request.exclude_rects,
            timeout_ms: request.timeout_ms,
            coalesce_ms: request.coalesce_ms,
            quiet_ms: request.quiet_ms,
            settle_max_ms: request.settle_max_ms,
        };
        let response = match self.request_wait_daemon(protocol_request, &context).await {
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
        description = "Execute 1 to 16 sequential input actions grounded in the supplied session-local frame number. Coordinates are pixels in that frame. Returns execution metadata plus a fresh numbered observation and PNG; batch only actions whose intermediate UI does not require inspection.",
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
        let request_id = self.action_request_id(&request_id);
        let mut state = self.state.lock().await;
        if context.ct.is_cancelled() {
            return Ok(cancelled_result());
        }
        let protocol_request = match state.prepare_action(&request_id, &request) {
            Ok(request) => request,
            Err(error) => {
                state.apply_error(&error);
                return Ok(structured_error(&error));
            }
        };
        let response = match self
            .request_daemon(request_id.clone(), DaemonRequest::Act(protocol_request))
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
                        "message": "action result omitted its observation",
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
        request_id: String,
        request: DaemonRequest,
    ) -> Result<ResponseEnvelope, CallToolResult> {
        match client::request(
            &self.socket,
            &RequestEnvelope {
                request_id,
                request,
            },
        )
        .await
        {
            Ok(response) => Ok(response),
            Err(error) => Err(CallToolResult::structured_error(json!({
                "code": "daemon_unavailable",
                "message": error.to_string(),
            }))),
        }
    }

    async fn request_wait_daemon(
        &self,
        request: WaitRequest,
        context: &RequestContext<RoleServer>,
    ) -> Result<ResponseEnvelope, CallToolResult> {
        let request = self.request_daemon(Uuid::new_v4().to_string(), DaemonRequest::Wait(request));
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
        match outcome.status {
            WaitStatus::Timeout => {
                if outcome.observation.is_some() || outcome.frame_id != baseline.internal_id {
                    self.state.lock().await.clear_if_current(&baseline);
                    return Ok(CallToolResult::structured_error(json!({
                        "code": "internal",
                        "message": "daemon returned an invalid wait timeout",
                    })));
                }
                let state = self.state.lock().await;
                if let Err(error) = state.ensure_current(&baseline) {
                    return Ok(structured_error(&error));
                }
                Ok(structured_result(McpWaitOutcome {
                    status: WaitStatus::Timeout,
                    elapsed_ms: outcome.elapsed_ms,
                    frame: baseline.frame,
                    activity_bbox: None,
                    observation: None,
                }))
            }
            WaitStatus::Changed => {
                let Some(observation) = outcome.observation.as_ref() else {
                    self.state.lock().await.clear_if_current(&baseline);
                    return Ok(CallToolResult::structured_error(json!({
                        "code": "internal",
                        "message": "changed wait result omitted its observation",
                    })));
                };
                if outcome.frame_id != observation.frame_id || outcome.activity_bbox.is_none() {
                    self.state.lock().await.clear_if_current(&baseline);
                    return Ok(CallToolResult::structured_error(json!({
                        "code": "internal",
                        "message": "daemon returned inconsistent changed wait metadata",
                    })));
                }

                let mut state = self.state.lock().await;
                let binding =
                    match state.bind_wait_observation(&baseline, observation.frame_id.clone()) {
                        Ok(binding) => binding,
                        Err(error) => return Ok(structured_error(&error)),
                    };
                let projected = McpObservation::from_protocol(observation, binding.frame);
                let result = image_result(
                    McpWaitOutcome {
                        status: WaitStatus::Changed,
                        elapsed_ms: outcome.elapsed_ms,
                        frame: binding.frame,
                        activity_bbox: outcome.activity_bbox,
                        observation: Some(projected),
                    },
                    observation.image_path.clone(),
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

    fn action_request_id(&self, request_id: &rmcp::model::RequestId) -> String {
        let typed_id = match request_id {
            rmcp::model::RequestId::Number(value) => format!("n:{value}"),
            rmcp::model::RequestId::String(value) => format!("s:{value}"),
        };
        format!("mcp:{}:{typed_id}", self.session_id)
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
            .with_instructions(self.instructions.clone())
    }

    async fn on_initialized(&self, _context: NotificationContext<RoleServer>) {
        eprintln!("computer-use MCP initialized");
    }
}

pub async fn serve(socket: PathBuf) -> anyhow::Result<()> {
    let profile = match tokio::time::timeout(PROFILE_FETCH_TIMEOUT, fetch_profile(&socket)).await {
        Ok(Ok(profile)) => profile,
        Ok(Err(error)) => {
            eprintln!("desktop profile unavailable; using generic MCP instructions: {error:#}");
            None
        }
        Err(_) => {
            eprintln!("desktop profile fetch timed out; using generic MCP instructions");
            None
        }
    };
    ComputerUseMcp::new(socket, profile.as_deref())
        .serve(stdio())
        .await?
        .waiting()
        .await?;
    Ok(())
}

fn compose_instructions(profile: Option<&str>) -> String {
    match profile {
        Some(profile) => format!("{MCP_INSTRUCTIONS}\n\nDesktop profile:\n{profile}"),
        None => MCP_INSTRUCTIONS.to_owned(),
    }
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

fn cancelled_result() -> CallToolResult {
    CallToolResult::structured_error(json!({
        "code": "cancelled",
        "message": "the computer operation was cancelled; call computer_observe before continuing",
    }))
}

fn stale_frame_result(message: &str) -> CallToolResult {
    structured_error(&CuError::new(ErrorCode::StaleFrame, message))
}

fn unexpected_response(operation: &str) -> CallToolResult {
    CallToolResult::structured_error(json!({
        "code": "internal",
        "message": format!("daemon returned the wrong response to computer_{operation}"),
    }))
}

fn structured_error(error: &impl Serialize) -> CallToolResult {
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
            return Ok(CallToolResult::structured_error(json!({
                "code": "image_unavailable",
                "message": format!("failed to read captured PNG: {error}"),
            })));
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
        let server = ComputerUseMcp::new(PathBuf::from("/tmp/not-used.sock"), None);
        let mut names = server
            .tool_router
            .list_all()
            .into_iter()
            .map(|tool| tool.name.into_owned())
            .collect::<Vec<_>>();
        names.sort();

        assert_eq!(names, ["computer_act", "computer_observe", "computer_wait"]);
    }

    #[test]
    fn identifies_itself_and_explains_the_complete_agent_loop() {
        let server = ComputerUseMcp::new(PathBuf::from("/tmp/not-used.sock"), None);
        let info = server.get_info();

        assert_eq!(info.server_info.name, "cu");
        assert_eq!(info.server_info.version, env!("CARGO_PKG_VERSION"));
        let instructions = info.instructions.unwrap();
        for required in [
            "computer_observe",
            "computer_wait",
            "latest returned frame number",
            "[0,width)",
            "stale_frame",
            "partial execution",
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
    fn appends_the_daemon_profile_to_initialization_instructions() {
        let server = ComputerUseMcp::new(
            PathBuf::from("/tmp/not-used.sock"),
            Some("# Desktop\n\nSuper+Left tiles the focused window left."),
        );
        let instructions = server.get_info().instructions.unwrap();

        assert!(instructions.starts_with(MCP_INSTRUCTIONS));
        assert!(instructions.contains("Desktop profile:\n# Desktop"));
        assert!(instructions.contains("Super+Left tiles the focused window left."));
    }

    #[test]
    fn publishes_described_observe_and_action_schemas() {
        let server = ComputerUseMcp::new(PathBuf::from("/tmp/not-used.sock"), None);
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
        assert!(
            act_output["properties"]["image_expired"]["description"]
                .as_str()
                .is_some_and(|description| description.contains("without repeating"))
        );
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
        let server = ComputerUseMcp::new(PathBuf::from("/tmp/not-used.sock"), None);
        let wait = tool(&server, "computer_wait");
        let wait_annotations = wait.annotations.unwrap();
        assert_eq!(wait_annotations.read_only_hint, Some(true));
        assert_eq!(wait_annotations.destructive_hint, Some(false));
        assert_eq!(wait_annotations.idempotent_hint, Some(false));

        let wait_schema = wait.input_schema;
        assert_eq!(wait_schema["properties"]["frame"]["minimum"], 1);
        assert_eq!(wait_schema["properties"]["include_rects"]["minItems"], 1);
        assert_eq!(wait_schema["properties"]["include_rects"]["maxItems"], 8);
        assert_eq!(wait_schema["properties"]["exclude_rects"]["maxItems"], 8);
        assert_eq!(wait_schema["properties"]["timeout_ms"]["maximum"], 600_000);
        assert_eq!(wait_schema["properties"]["coalesce_ms"]["default"], 2_000);
        assert_eq!(wait_schema["properties"]["quiet_ms"]["default"], 1_000);
        assert_eq!(
            wait_schema["properties"]["settle_max_ms"]["default"],
            15_000
        );

        let wait_output = wait.output_schema.unwrap();
        assert_eq!(wait_output["properties"]["elapsed_ms"]["type"], "integer");
        assert_eq!(wait_output["properties"]["frame"]["minimum"], 1);
        assert!(wait_output["properties"].get("frame_id").is_none());
        let wait_observation_properties = &wait_output["$defs"]["McpObservation"]["properties"];
        for omitted in ["frame_id", "image_path", "target", "coordinate_space"] {
            assert!(
                wait_observation_properties.get(omitted).is_none(),
                "wait output exposes {omitted}"
            );
        }
    }

    #[test]
    fn normal_action_result_contains_every_schema_required_field() {
        let server = ComputerUseMcp::new(PathBuf::from("/tmp/not-used.sock"), None);
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
        let server = ComputerUseMcp::new(directory.path().join("missing.sock"), None);

        let Err(result) = server
            .request_daemon(
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
                status: WaitStatus::Changed,
                elapsed_ms: 32_784,
                frame: 2,
                activity_bbox: Some(Rect {
                    x: 120,
                    y: 240,
                    width: 600,
                    height: 310,
                }),
                observation: Some(McpObservation {
                    frame: 2,
                    width: 1_920,
                    height: 1_080,
                    settled: true,
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
        assert_eq!(structured["observation"]["frame"], 2);
        assert_eq!(result.content.len(), 2);
        assert!(result.content[1].as_image().is_some());
    }

    #[test]
    fn timed_out_wait_preserves_its_frame_without_an_image() {
        let result = structured_result(McpWaitOutcome {
            status: WaitStatus::Timeout,
            elapsed_ms: 30_012,
            frame: 7,
            activity_bbox: None,
            observation: None,
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
        let server = ComputerUseMcp::new(PathBuf::from("/tmp/not-used.sock"), None);
        let wait_output = tool(&server, "computer_wait").output_schema.unwrap();
        let outcomes = [
            McpWaitOutcome {
                status: WaitStatus::Timeout,
                elapsed_ms: 30_000,
                frame: 1,
                activity_bbox: None,
                observation: None,
            },
            McpWaitOutcome {
                status: WaitStatus::Changed,
                elapsed_ms: 2_500,
                frame: 2,
                activity_bbox: Some(Rect {
                    x: 10,
                    y: 20,
                    width: 30,
                    height: 40,
                }),
                observation: Some(McpObservation {
                    frame: 2,
                    width: 100,
                    height: 80,
                    settled: true,
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
        let server = ComputerUseMcp::new(PathBuf::from("/tmp/not-used.sock"), None);

        let numeric = server.action_request_id(&rmcp::model::RequestId::Number(7));
        let string = server.action_request_id(&rmcp::model::RequestId::String("7".into()));

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
