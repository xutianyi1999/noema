use axum::{
    Json, Router,
    extract::{Path, State},
    routing::{get, post},
};

use crate::{
    error::AppError,
    models::{
        CreateLibraryRequest, DocumentInput, HealthResponse, JobStatus, Library, QueryRequest,
        QueryResponse, SubmitDocumentResponse,
    },
    runtime::OpenCodeRuntime,
    service::AppService,
};

pub fn router<R: OpenCodeRuntime>(service: AppService<R>) -> Router {
    Router::new()
        .route("/v1/health", get(health::<R>))
        .route("/v1/libraries", post(create_library::<R>))
        .route(
            "/v1/libraries/{library_id}/documents",
            post(submit_document::<R>),
        )
        .route(
            "/v1/libraries/{library_id}/jobs/{job_id}",
            get(job_status::<R>),
        )
        .route("/v1/libraries/{library_id}/query", post(query::<R>))
        .nest_service("/mcp", crate::mcp::streamable_http_service(service.clone()))
        .with_state(service)
}

async fn health<R: OpenCodeRuntime>(State(service): State<AppService<R>>) -> Json<HealthResponse> {
    Json(service.health())
}

async fn create_library<R: OpenCodeRuntime>(
    State(service): State<AppService<R>>,
    Json(request): Json<CreateLibraryRequest>,
) -> Result<Json<Library>, AppError> {
    Ok(Json(service.create_library(request).await?))
}

async fn submit_document<R: OpenCodeRuntime>(
    State(service): State<AppService<R>>,
    Path(library_id): Path<String>,
    Json(document): Json<DocumentInput>,
) -> Result<Json<SubmitDocumentResponse>, AppError> {
    Ok(Json(service.submit_document(&library_id, document).await?))
}

async fn job_status<R: OpenCodeRuntime>(
    State(service): State<AppService<R>>,
    Path((library_id, job_id)): Path<(String, String)>,
) -> Result<Json<JobStatus>, AppError> {
    Ok(Json(service.job_status(&library_id, &job_id)?))
}

async fn query<R: OpenCodeRuntime>(
    State(service): State<AppService<R>>,
    Path(library_id): Path<String>,
    Json(request): Json<QueryRequest>,
) -> Result<Json<QueryResponse>, AppError> {
    Ok(Json(service.query(&library_id, &request.prompt).await?))
}
