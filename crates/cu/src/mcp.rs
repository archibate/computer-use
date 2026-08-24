use std::path::PathBuf;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use cu_protocol::{
    ActRequest, DaemonRequest, DaemonResponse, ObserveRequest, RequestEnvelope, ResponseEnvelope,
    ResponseResult,
};
use rmcp::{
    ErrorData as McpError, RoleServer, ServerHandler, ServiceExt,
    handler::server::{tool::RequestId, wrapper::Parameters},
    model::{CallToolResult, ContentBlock, ServerCapabilities, ServerInfo},
    service::NotificationContext,
    tool, tool_handler, tool_router,
    transport::stdio,
};
use uuid::Uuid;

use crate::client;

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
        description = "Capture the configured computer target. Use before the first action and whenever the current frame is unknown. Later computer_act coordinates refer to this frame."
    )]
    async fn computer_observe(&self) -> Result<CallToolResult, McpError> {
        self.call(
            Uuid::new_v4().to_string(),
            DaemonRequest::Observe(ObserveRequest::default()),
        )
        .await
    }

    #[tool(
        description = "Execute up to 16 actions from the expected frame, then return the resulting screenshot. Batch only steps that do not require inspecting intermediate UI."
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
                return Ok(CallToolResult::error(vec![ContentBlock::text(
                    error.to_string(),
                )]));
            }
        };
        to_tool_result(response).await
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for ComputerUseMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
            "Observe, reason over the returned screenshot, then act using its frame_id.",
        )
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
        ResponseResult::Error(error) => Ok(CallToolResult::error(vec![ContentBlock::text(
            serde_json::to_string(&error).unwrap_or(error.message),
        )])),
        ResponseResult::Ok(DaemonResponse::Observe(observation)) => {
            let image_path = observation.image_path.clone();
            let metadata = match serde_json::to_string(&observation) {
                Ok(metadata) => metadata,
                Err(error) => {
                    return Ok(CallToolResult::error(vec![ContentBlock::text(
                        error.to_string(),
                    )]));
                }
            };
            image_result(metadata, image_path).await
        }
        ResponseResult::Ok(DaemonResponse::Act(outcome)) => {
            let image_path = outcome.observation.image_path.clone();
            let metadata = match serde_json::to_string(&outcome) {
                Ok(metadata) => metadata,
                Err(error) => {
                    return Ok(CallToolResult::error(vec![ContentBlock::text(
                        error.to_string(),
                    )]));
                }
            };
            image_result(metadata, image_path).await
        }
    }
}

async fn image_result(metadata: String, image_path: String) -> Result<CallToolResult, McpError> {
    let image = match tokio::fs::read(&image_path).await {
        Ok(image) => image,
        Err(error) => {
            return Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                "failed to read {image_path}: {error}"
            ))]));
        }
    };
    Ok(CallToolResult::success(vec![
        ContentBlock::text(metadata),
        ContentBlock::image(STANDARD.encode(image), "image/png"),
    ]))
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
