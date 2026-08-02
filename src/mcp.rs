use rmcp::{
    ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{Implementation, ServerCapabilities, ServerInfo},
    tool, tool_handler, tool_router,
    transport::{
        StreamableHttpServerConfig, StreamableHttpService,
        streamable_http_server::session::local::LocalSessionManager,
    },
};

use crate::{
    models::{DocumentInput, McpIngestRequest, McpJobRequest, McpQueryRequest},
    runtime::OpenCodeRuntime,
    service::AppService,
};

#[derive(Clone)]
pub struct McpHandler<R: OpenCodeRuntime> {
    service: AppService<R>,
    tool_router: ToolRouter<Self>,
}

impl<R: OpenCodeRuntime> McpHandler<R> {
    fn new(service: AppService<R>) -> Self {
        Self {
            service,
            tool_router: Self::tool_router(),
        }
    }
}

#[tool_router]
impl<R: OpenCodeRuntime> McpHandler<R> {
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
            .map_err(|error| to_mcp_error(&error))?;
        json_response(&response)
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
            .map_err(|error| to_mcp_error(&error))?;
        json_response(&response)
    }

    #[tool(description = "Get the status of an ingestion job in one isolated content library.")]
    async fn kb_job_status(
        &self,
        Parameters(request): Parameters<McpJobRequest>,
    ) -> Result<String, rmcp::ErrorData> {
        let response = self
            .service
            .job_status(&request.library_id, &request.job_id)
            .map_err(|error| to_mcp_error(&error))?;
        json_response(&response)
    }

    #[tool(description = "Return Noema service health and the configured OpenCode model.")]
    async fn kb_health(&self) -> Result<String, rmcp::ErrorData> {
        json_response(&self.service.health())
    }
}

#[tool_handler(router = self.tool_router)]
impl<R: OpenCodeRuntime> ServerHandler for McpHandler<R> {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(
                Implementation::new("noema", env!("CARGO_PKG_VERSION"))
                    .with_title("Noema")
                    .with_description("OpenCode-driven isolated text knowledge-base service"),
            )
            .with_instructions(
                "Every tool takes an explicit library_id; content libraries are isolated. \
                 kb_query accepts only a natural-language prompt and always runs in a fresh \
                 OpenCode session.",
            )
    }
}

pub fn streamable_http_service<R: OpenCodeRuntime>(
    service: AppService<R>,
) -> StreamableHttpService<McpHandler<R>, LocalSessionManager> {
    let config = StreamableHttpServerConfig::default().with_json_response(true);
    StreamableHttpService::new(
        move || Ok(McpHandler::new(service.clone())),
        Default::default(),
        config,
    )
}

/// Serialize one successful tool result to the JSON string MCP tools
/// return, mapping any serialization failure onto the internal error code.
fn json_response<T: serde::Serialize>(value: &T) -> Result<String, rmcp::ErrorData> {
    serde_json::to_string(value).map_err(|error| to_mcp_error(&crate::AppError::from(error)))
}

/// Maps service errors onto MCP error codes: caller mistakes (unknown
/// library/job, invalid input) are invalid-params errors, not internal ones;
/// only genuine runtime/storage failures stay internal.
fn to_mcp_error(error: &crate::AppError) -> rmcp::ErrorData {
    match error {
        crate::AppError::BadRequest(_)
        | crate::AppError::LibraryNotFound(_)
        | crate::AppError::JobNotFound(_) => {
            rmcp::ErrorData::invalid_params(error.to_string(), None)
        }
        crate::AppError::QueryFailed(_)
        | crate::AppError::Runtime(_)
        | crate::AppError::Storage(_)
        | crate::AppError::Io(_)
        | crate::AppError::Sqlite(_)
        | crate::AppError::Json(_) => rmcp::ErrorData::internal_error(error.to_string(), None),
    }
}
