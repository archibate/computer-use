use std::path::PathBuf;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use cu_protocol::{
    ActRequest, DaemonRequest, DaemonResponse, ObserveRequest, RequestEnvelope, ResponseEnvelope,
    ResponseResult,
};
use rmcp::{
    ErrorData as McpError, RoleServer, ServerHandler, ServiceExt,
    handler::server::{tool::RequestId, wrapper::Parameters},
    model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerInfo},
    service::NotificationContext,
    tool, tool_handler, tool_router,
    transport::stdio,
};
use serde::Serialize;
use serde_json::json;
use uuid::Uuid;

use crate::client;

const MCP_INSTRUCTIONS: &str = "Use computer_observe before the first action and whenever the frame is unknown. Use only the latest returned frame_id as computer_act.expected_frame_id; x and y are integer pixels in [0,width) and [0,height). computer_act affects the live desktop and returns a fresh observation; ground the next action in that image. Batch actions only when no intermediate inspection is needed. After stale_frame, observe again. After partial execution, inspect the returned observation before continuing. Apply your authorization policy before consequential UI actions.";

#[derive(Clone)]
pub struct ComputerUseMcp {
    socket: PathBuf,
    session_id: Uuid,
    tool_router: rmcp::handler::server::tool::ToolRouter<Self>,
}

#[tool_router(router = tool_router)]
impl ComputerUseMcp {
    pub fn new(socket: PathBuf) -> Self {
        Self {
            socket,
            session_id: Uuid::new_v4(),
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        description = "Capture the current desktop after optional visual settling. Returns structured frame metadata plus a PNG image. Call before the first action, after stale_frame, or whenever UI state is uncertain; use its frame_id and image dimensions with computer_act.",
        output_schema = rmcp::handler::server::tool::schema_for_output::<cu_protocol::Observation>(),
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
        output_schema = rmcp::handler::server::tool::schema_for_output::<cu_protocol::ActOutcome>(),
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
            .with_instructions(MCP_INSTRUCTIONS)
    }

    async fn on_initialized(&self, _context: NotificationContext<RoleServer>) {
        eprintln!("computer-use MCP initialized");
    }
}

pub async fn serve(socket: PathBuf) -> anyhow::Result<()> {
    ComputerUseMcp::new(socket)
        .serve(stdio())
        .await?
        .waiting()
        .await?;
    Ok(())
}

async fn to_tool_result(response: ResponseEnvelope) -> Result<CallToolResult, McpError> {
    match response.result {
        ResponseResult::Error(error) => Ok(structured_error(&error)),
        ResponseResult::Ok(DaemonResponse::Observe(observation)) => {
            let image_path = observation.image_path.clone();
            image_result(observation, image_path).await
        }
        ResponseResult::Ok(DaemonResponse::Act(outcome)) => {
            let image_path = outcome.observation.image_path.clone();
            image_result(outcome, image_path).await
        }
    }
}

fn structured_error(error: &impl Serialize) -> CallToolResult {
    match serde_json::to_value(error) {
        Ok(value) => CallToolResult::structured_error(value),
        Err(error) => CallToolResult::error(vec![ContentBlock::text(error.to_string())]),
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
                "message": format!("failed to read {image_path}: {error}"),
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
        let server = ComputerUseMcp::new(PathBuf::from("/tmp/not-used.sock"));
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
        let server = ComputerUseMcp::new(PathBuf::from("/tmp/not-used.sock"));
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
    fn publishes_described_bounded_input_and_structured_output_schemas() {
        let server = ComputerUseMcp::new(PathBuf::from("/tmp/not-used.sock"));
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
        let act_output = act.output_schema.unwrap();
        assert!(
            act_output["properties"]["observation"]["description"]
                .as_str()
                .is_some_and(|description| description.contains("subsequent reasoning"))
        );
    }

    #[tokio::test]
    async fn missing_daemon_returns_a_start_command_to_the_agent() {
        let directory = TempDir::new().unwrap();
        let server = ComputerUseMcp::new(directory.path().join("missing.sock"));

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
        let expected = serde_json::to_value(&observation).unwrap();

        let result = to_tool_result(ResponseEnvelope {
            request_id: "structured-observe".to_owned(),
            result: ResponseResult::Ok(DaemonResponse::Observe(observation)),
        })
        .await
        .unwrap();

        assert_eq!(result.is_error, Some(false));
        assert_eq!(result.structured_content, Some(expected));
        assert_eq!(result.content.len(), 2);
        assert!(result.content[0].as_text().is_some());
        assert!(result.content[1].as_image().is_some());
    }
}
