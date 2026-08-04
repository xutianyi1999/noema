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
        CreateLibraryRequest, McpEnsureLibraryRequest, McpIngestRequest, McpJobRequest,
        McpQueryRequest,
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
    #[tool(
        description = "确保指定的隔离内容库已经存在；已存在时直接返回，不会重复创建。Agent 应在首次写入前调用它。"
    )]
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

    #[tool(
        description = "向隔离的 Noema 内容库提交一篇或多篇 UTF-8 Markdown/TXT 文档。所有文档会合并到同一个摄入作业中编译。"
    )]
    async fn kb_ingest_documents(
        &self,
        Parameters(request): Parameters<McpIngestRequest>,
    ) -> Result<String, rmcp::ErrorData> {
        let response = self
            .service
            .submit_documents(&request.library_id, request.documents)
            .await
            .map_err(|error| to_mcp_error(&error))?;
        json_response(&response)
    }

    #[tool(
        description = "使用自然语言提示词查询一个隔离的 Noema 内容库。省略 session_id 时创建会话；传入同一内容库此前成功查询返回的 session_id 时继续该会话。"
    )]
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

    #[tool(description = "获取一个隔离内容库中的摄入作业状态。")]
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

    #[tool(description = "返回 Noema 服务健康状态和已配置的 OpenCode 模型。")]
    async fn kb_health(&self) -> Result<String, rmcp::ErrorData> {
        json_response(&self.service.health(true))
    }
}

#[tool_handler(router = self.tool_router)]
impl<R: OpenCodeRuntime> ServerHandler for McpHandler<R> {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(
                Implementation::new("noema", env!("CARGO_PKG_VERSION"))
                    .with_title("Noema")
                    .with_description("由 OpenCode 驱动的隔离文本知识库服务"),
            )
            .with_instructions(
                "每个工具都必须显式传入 library_id；内容库彼此隔离。省略 session_id 时，kb_query 创建新的 OpenCode 会话；传入同一内容库此前成功查询的 session_id 时继续该会话。",
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
