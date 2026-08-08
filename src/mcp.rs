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
    models::{
        CreateLibraryRequest, McpDeleteDocumentRequest, McpEnsureLibraryRequest, McpJobRequest,
        McpListDocumentsRequest, McpQueryRequest,
    },
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
    #[tool(description = "创建指定内容库；已存在时返回现有内容库。")]
    async fn kb_ensure_library(
        &self,
        Parameters(request): Parameters<McpEnsureLibraryRequest>,
    ) -> Result<String, rmcp::ErrorData> {
        let response = self
            .service
            .ensure_library(CreateLibraryRequest {
                name: request.name,
                description: request.description,
            })
            .await
            .map_err(|error| to_mcp_error(&error))?;
        json_response(&response)
    }

    #[tool(description = "使用自然语言查询内容库；可传入 session_id 继续已有会话。")]
    async fn kb_query(
        &self,
        Parameters(request): Parameters<McpQueryRequest>,
    ) -> Result<String, rmcp::ErrorData> {
        let response = self
            .service
            .query(
                &request.library_id,
                &request.prompt,
                request.session_id.as_deref(),
            )
            .await
            .map_err(|error| to_mcp_error(&error))?;
        json_response(&response)
    }

    #[tool(description = "获取内容库摄入或维护作业的状态。")]
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

    #[tool(description = "列出内容库中的文档元数据。")]
    async fn kb_list_documents(
        &self,
        Parameters(request): Parameters<McpListDocumentsRequest>,
    ) -> Result<String, rmcp::ErrorData> {
        let response = self
            .service
            .list_documents(&request.library_id)
            .map_err(|error| to_mcp_error(&error))?;
        json_response(&response)
    }

    #[tool(description = "删除一篇文档并创建维护作业。")]
    async fn kb_delete_document(
        &self,
        Parameters(request): Parameters<McpDeleteDocumentRequest>,
    ) -> Result<String, rmcp::ErrorData> {
        let response = self
            .service
            .delete_document(&request.library_id, &request.filename)
            .await
            .map_err(|error| to_mcp_error(&error))?;
        json_response(&response)
    }

    #[tool(description = "返回 Noema 服务健康状态和已配置的 OpenCode 模型。")]
    async fn kb_health(&self) -> Result<String, rmcp::ErrorData> {
        json_response(&self.service.health(true))
    }
}

#[tool_handler(router = self.tool_router)]
impl<R: OpenCodeRuntime> ServerHandler for McpHandler<R> {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_server_info(
            Implementation::new("noema", env!("CARGO_PKG_VERSION"))
                .with_title("Noema")
                .with_description("由 OpenCode 驱动的文本知识库服务"),
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
        // Unauthorized is rejected at the HTTP layer before reaching the MCP
        // tools, so this arm is only a completeness mapping.
        crate::AppError::BadRequest(_)
        | crate::AppError::Unauthorized
        | crate::AppError::Conflict(_)
        | crate::AppError::LibraryNotFound(_)
        | crate::AppError::JobNotFound(_)
        | crate::AppError::FileNotFound(_) => {
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
