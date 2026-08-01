use rmcp::{
    ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{ServerCapabilities, ServerInfo},
    tool, tool_handler, tool_router,
    transport::{
        StreamableHttpServerConfig, StreamableHttpService,
        streamable_http_server::session::local::LocalSessionManager,
    },
};

use crate::{
    models::{DocumentInput, McpIngestRequest, McpJobRequest, McpQueryRequest},
    service::AppService,
};

#[derive(Clone)]
pub struct McpHandler {
    service: AppService,
    tool_router: ToolRouter<Self>,
}

impl McpHandler {
    fn new(service: AppService) -> Self {
        Self {
            service,
            tool_router: Self::tool_router(),
        }
    }
}

#[tool_router]
impl McpHandler {
    #[tool(
        description = "Submit one UTF-8 Markdown or TXT document to an isolated Noema content library."
    )]
    async fn kb_ingest_document(
        &self,
        Parameters(request): Parameters<McpIngestRequest>,
    ) -> Result<String, rmcp::ErrorData> {
        let response = self
            .service
            .submit_document(
                &request.library_id,
                DocumentInput {
                    filename: request.filename,
                    content: request.content,
                    title: request.title,
                },
            )
            .await
            .map_err(to_mcp_error)?;
        serde_json::to_string(&response).map_err(to_mcp_error)
    }

    #[tool(
        description = "Query one isolated Noema content library with a natural-language prompt. Every call creates a new OpenCode session."
    )]
    async fn kb_query(
        &self,
        Parameters(request): Parameters<McpQueryRequest>,
    ) -> Result<String, rmcp::ErrorData> {
        let response = self
            .service
            .query(&request.library_id, &request.prompt)
            .await
            .map_err(to_mcp_error)?;
        serde_json::to_string(&response).map_err(to_mcp_error)
    }

    #[tool(description = "Get the status of an ingestion job in one isolated content library.")]
    async fn kb_job_status(
        &self,
        Parameters(request): Parameters<McpJobRequest>,
    ) -> Result<String, rmcp::ErrorData> {
        let response = self
            .service
            .job_status(&request.library_id, &request.job_id)
            .map_err(to_mcp_error)?;
        serde_json::to_string(&response).map_err(to_mcp_error)
    }

    #[tool(description = "Return Noema service health and the configured OpenCode model.")]
    async fn kb_health(&self) -> Result<String, rmcp::ErrorData> {
        serde_json::to_string(&self.service.health()).map_err(to_mcp_error)
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for McpHandler {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
    }
}

pub fn streamable_http_service(
    service: AppService,
) -> StreamableHttpService<McpHandler, LocalSessionManager> {
    let config = StreamableHttpServerConfig::default().with_json_response(true);
    StreamableHttpService::new(
        move || Ok(McpHandler::new(service.clone())),
        Default::default(),
        config,
    )
}

fn to_mcp_error(error: impl std::fmt::Display) -> rmcp::ErrorData {
    rmcp::ErrorData::internal_error(error.to_string(), None)
}
