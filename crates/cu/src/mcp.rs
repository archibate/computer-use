use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use cu_protocol::{
    ActOutcome, ActRequest, ActStatus, CuError, DaemonRequest, DaemonResponse, ErrorCode,
    Observation, ObserveRequest, RequestEnvelope, ResponseEnvelope, ResponseResult,
};
use rmcp::{
    ErrorData as McpError, RoleServer, ServerHandler, ServiceExt,
    handler::server::{tool::RequestId, wrapper::Parameters},
    model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerInfo},
    service::NotificationContext,
    tool, tool_handler, tool_router,
    transport::stdio,
};
use schemars::JsonSchema;
use serde::Serialize;
use serde_json::json;
use uuid::Uuid;

use crate::client;

const MCP_INSTRUCTIONS: &str = "Use computer_observe before the first action and whenever the frame is unknown. Use only the latest returned frame_id as computer_act.expected_frame_id; x and y are integer pixels in [0,width) and [0,height). computer_act affects the live desktop and returns a fresh observation; ground the next action in that image. Batch actions only when no intermediate inspection is needed. After stale_frame, observe again. If a cached action replay says image_expired, the recorded action already executed: do not repeat it; observe again. After partial execution, inspect the returned observation before continuing. Apply your authorization policy before consequential UI actions.";
const PROFILE_FETCH_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Serialize, JsonSchema)]
struct McpObservation {
    /// Opaque frame identifier required by the next `computer_act` call.
    frame_id: String,
    /// PNG width and exclusive upper bound for action x coordinates.
    width: u32,
    /// PNG height and exclusive upper bound for action y coordinates.
    height: u32,
    /// Whether the screen stayed unchanged for `quiet_ms`; an unsettled frame remains usable.
    settled: bool,
}

impl From<Observation> for McpObservation {
    fn from(observation: Observation) -> Self {
        Self {
            frame_id: observation.frame_id,
            width: observation.width,
            height: observation.height,
            settled: observation.settled,
        }
    }
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

impl From<ActOutcome> for McpActOutcome {
    fn from(outcome: ActOutcome) -> Self {
        let ActOutcome {
            status,
            executed,
            action_error,
            image_expired,
            observation,
        } = outcome;
        Self {
            status,
            executed,
            action_error: action_error.map(|error| McpActionError::from_protocol(error, executed)),
            image_expired,
            observation: observation.map(Into::into),
        }
    }
}

#[derive(Clone)]
pub struct ComputerUseMcp {
    socket: PathBuf,
    instructions: String,
    session_id: Uuid,
    tool_router: rmcp::handler::server::tool::ToolRouter<Self>,
}

#[tool_router(router = tool_router)]
impl ComputerUseMcp {
    pub fn new(socket: PathBuf, profile: Option<&str>) -> Self {
        Self {
            socket,
            instructions: compose_instructions(profile),
            session_id: Uuid::new_v4(),
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        description = "Capture the current desktop after optional visual settling. Returns structured frame metadata plus a PNG image. Call before the first action, after stale_frame, or whenever UI state is uncertain; use its frame_id and image dimensions with computer_act.",
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
    ) -> Result<CallToolResult, McpError> {
        self.call(Uuid::new_v4().to_string(), DaemonRequest::Observe(request))
            .await
    }

    #[tool(
        description = "Execute 1 to 16 sequential input actions grounded in expected_frame_id. Coordinates are pixels in that frame. Returns structured execution metadata plus a fresh PNG; batch only actions whose intermediate UI does not require inspection.",
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
        Parameters(request): Parameters<ActRequest>,
        RequestId(request_id): RequestId,
    ) -> Result<CallToolResult, McpError> {
        self.call(
            format!("mcp:{}:{request_id}", self.session_id),
            DaemonRequest::Act(request),
        )
        .await
    }

    async fn call(
        &self,
        request_id: String,
        request: DaemonRequest,
    ) -> Result<CallToolResult, McpError> {
        let response = match client::request(
            &self.socket,
            &RequestEnvelope {
                request_id,
                request,
            },
        )
        .await
        {
            Ok(response) => response,
            Err(error) => {
                return Ok(CallToolResult::structured_error(json!({
                    "code": "daemon_unavailable",
                    "message": error.to_string(),
                })));
            }
        };
        to_tool_result(response).await
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

async fn to_tool_result(response: ResponseEnvelope) -> Result<CallToolResult, McpError> {
    match response.result {
        ResponseResult::Error(error) => Ok(structured_error(&error)),
        ResponseResult::Ok(DaemonResponse::Observe(observation)) => {
            let image_path = observation.image_path.clone();
            image_result(McpObservation::from(observation), image_path).await
        }
        ResponseResult::Ok(DaemonResponse::Act(outcome)) => {
            let image_path = outcome
                .observation
                .as_ref()
                .map(|observation| observation.image_path.clone());
            let metadata = McpActOutcome::from(outcome);
            match image_path {
                Some(image_path) => image_result(metadata, image_path).await,
                None if metadata.image_expired => Ok(structured_result(metadata)),
                None => Ok(CallToolResult::structured_error(json!({
                    "code": "internal",
                    "message": "action result omitted its observation",
                }))),
            }
        }
        ResponseResult::Ok(DaemonResponse::Profile(_)) => {
            Ok(CallToolResult::structured_error(json!({
                "code": "internal",
                "message": "profile response reached a computer tool",
            })))
        }
    }
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

        assert_eq!(names, ["computer_act", "computer_observe"]);
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
            "latest returned frame_id",
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
    fn publishes_described_bounded_input_and_structured_output_schemas() {
        let server = ComputerUseMcp::new(PathBuf::from("/tmp/not-used.sock"), None);
        let observe = tool(&server, "computer_observe");
        let act = tool(&server, "computer_act");

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
        assert!(
            schema["properties"]["expected_frame_id"]["description"]
                .as_str()
                .is_some_and(|description| description.contains("stale_frame"))
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
        assert!(
            observe_output["properties"]["frame_id"]["description"]
                .as_str()
                .is_some_and(|description| description.contains("computer_act"))
        );
        for omitted in ["image_path", "target", "coordinate_space"] {
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
        for omitted in ["image_path", "target", "coordinate_space"] {
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
    fn normal_action_result_contains_every_schema_required_field() {
        let server = ComputerUseMcp::new(PathBuf::from("/tmp/not-used.sock"), None);
        let act_output = tool(&server, "computer_act").output_schema.unwrap();
        let normal = serde_json::to_value(McpActOutcome {
            status: ActStatus::Ok,
            executed: 1,
            action_error: None,
            image_expired: false,
            observation: Some(McpObservation {
                frame_id: "f_schema".to_owned(),
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

        let result = server
            .call(
                "missing-daemon-test".to_owned(),
                DaemonRequest::Observe(ObserveRequest::default()),
            )
            .await
            .unwrap();

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
            "frame_id": "f_test_1",
            "width": 100,
            "height": 80,
            "settled": true,
        });

        let result = to_tool_result(ResponseEnvelope {
            request_id: "structured-observe".to_owned(),
            result: ResponseResult::Ok(DaemonResponse::Observe(observation)),
        })
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
    async fn unavailable_image_error_omits_the_private_path() {
        let directory = TempDir::new().unwrap();
        let image_path = directory.path().join("missing-frame.png");
        let private_path = image_path.to_string_lossy().into_owned();

        let result = image_result(json!({"frame_id": "f_test_missing"}), private_path.clone())
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

        let result = to_tool_result(ResponseEnvelope {
            request_id: "structured-act".to_owned(),
            result: ResponseResult::Ok(DaemonResponse::Act(outcome)),
        })
        .await
        .unwrap();

        let structured = result.structured_content.as_ref().unwrap();
        assert_eq!(structured["status"], "ok");
        assert_eq!(structured["executed"], 1);
        assert_eq!(structured["observation"]["frame_id"], "f_test_2");
        for omitted in ["image_path", "target", "coordinate_space"] {
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

        let result = to_tool_result(ResponseEnvelope {
            request_id: "structured-partial-act".to_owned(),
            result: ResponseResult::Ok(DaemonResponse::Act(outcome)),
        })
        .await
        .unwrap();

        let structured = result.structured_content.as_ref().unwrap();
        assert_eq!(structured["status"], "partial");
        assert_eq!(structured["executed"], 1);
        assert_eq!(structured["action_error"]["code"], "input_failed");
        assert_eq!(structured["action_error"]["executed"], 1);
        assert!(structured["action_error"]["executed"].is_number());
        assert_eq!(structured["observation"]["frame_id"], "f_test_partial");
        assert_eq!(result.content.len(), 2);
        assert!(result.content[1].as_image().is_some());
    }

    #[tokio::test]
    async fn expired_cached_action_is_a_non_error_without_an_image() {
        let result = to_tool_result(ResponseEnvelope {
            request_id: "expired-replay".to_owned(),
            result: ResponseResult::Ok(DaemonResponse::Act(ActOutcome {
                status: ActStatus::Ok,
                executed: 1,
                action_error: None,
                image_expired: true,
                observation: None,
            })),
        })
        .await
        .unwrap();

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
}
