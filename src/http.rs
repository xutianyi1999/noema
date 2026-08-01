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
    service::AppService,
};

pub fn router(service: AppService) -> Router {
    Router::new()
        .route("/v1/health", get(health))
        .route("/v1/libraries", post(create_library))
        .route(
            "/v1/libraries/{library_id}/documents",
            post(submit_document),
        )
        .route("/v1/libraries/{library_id}/jobs/{job_id}", get(job_status))
        .route("/v1/libraries/{library_id}/query", post(query))
        .nest_service("/mcp", crate::mcp::streamable_http_service(service.clone()))
        .with_state(service)
}

async fn health(State(service): State<AppService>) -> Json<HealthResponse> {
    Json(service.health())
}

async fn create_library(
    State(service): State<AppService>,
    Json(request): Json<CreateLibraryRequest>,
) -> Result<Json<Library>, AppError> {
    Ok(Json(service.create_library(request).await?))
}

async fn submit_document(
    State(service): State<AppService>,
    Path(library_id): Path<String>,
    Json(document): Json<DocumentInput>,
) -> Result<Json<SubmitDocumentResponse>, AppError> {
    Ok(Json(service.submit_document(&library_id, document).await?))
}

async fn job_status(
    State(service): State<AppService>,
    Path((library_id, job_id)): Path<(String, String)>,
) -> Result<Json<JobStatus>, AppError> {
    Ok(Json(service.job_status(&library_id, &job_id)?))
}

async fn query(
    State(service): State<AppService>,
    Path(library_id): Path<String>,
    Json(request): Json<QueryRequest>,
) -> Result<Json<QueryResponse>, AppError> {
    Ok(Json(service.query(&library_id, &request.prompt).await?))
}
