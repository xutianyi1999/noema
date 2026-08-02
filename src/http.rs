use std::{
    io,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use axum::{
    Json, Router,
    body::Body,
    extract::{Path, Query, Request, State},
    http::{StatusCode, header},
    middleware,
    response::Response,
    routing::{get, post},
};
use bytes::Bytes;
use futures_util::{Stream, TryStreamExt};
use serde::Deserialize;
use subtle::ConstantTimeEq;
use tokio_util::io::{ReaderStream, StreamReader};

use crate::{
    error::AppError,
    models::{
        CreateLibraryRequest, DocumentInput, HealthResponse, JobStatus, Library, QueryRequest,
        QueryResponse, SubmitDocumentResponse,
    },
    runtime::OpenCodeRuntime,
    service::AppService,
    snapshot,
};

pub fn router<R: OpenCodeRuntime>(service: AppService<R>) -> Router {
    let auth_token = service.auth_token().map(Arc::from);
    let router = Router::new()
        .route("/v1/health", get(health::<R>))
        .route(
            "/v1/libraries",
            get(list_libraries::<R>).post(create_library::<R>),
        )
        .route("/v1/libraries/import", post(import_library::<R>))
        .route(
            "/v1/libraries/{library_id}/export",
            get(export_library::<R>),
        )
        .route(
            "/v1/libraries/{library_id}/documents",
            post(submit_document::<R>),
        )
        .route(
            "/v1/libraries/{library_id}/jobs/{job_id}",
            get(job_status::<R>),
        )
        .route("/v1/libraries/{library_id}/query", post(query::<R>))
        .route(
            "/v1/libraries/{library_id}/files/{*path}",
            get(knowledge_file::<R>),
        )
        .nest_service("/mcp", crate::mcp::streamable_http_service(service.clone()))
        .with_state(service);
    // Without a configured token the API stays open: the historical
    // loopback-only deployment model. With one, every route (including the
    // MCP endpoint) except the unauthenticated health probe requires it.
    match auth_token {
        Some(token) => router.layer(middleware::from_fn_with_state(token, require_auth)),
        None => router,
    }
}

/// Bearer-token guard: rejects requests whose `Authorization` header is
/// absent, malformed, or carries the wrong token with `401 Unauthorized`.
async fn require_auth(
    State(token): State<Arc<str>>,
    request: Request,
    next: middleware::Next,
) -> Result<Response, AppError> {
    // Container orchestrators probe health without credentials.
    if request.uri().path() == "/v1/health" {
        return Ok(next.run(request).await);
    }
    let presented = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    match presented {
        Some(candidate) if tokens_match(candidate, &token) => Ok(next.run(request).await),
        _ => Err(AppError::Unauthorized),
    }
}

/// Constant-time string equality, so response timing never reveals how many
/// leading bytes of a guessed token were correct.
fn tokens_match(candidate: &str, expected: &str) -> bool {
    candidate.as_bytes().ct_eq(expected.as_bytes()).into()
}

#[cfg(test)]
mod tests {
    use super::tokens_match;

    #[test]
    fn token_comparison_is_exact() {
        assert!(tokens_match("s3cret", "s3cret"));
        assert!(!tokens_match("s3cret", "s3creT"));
        assert!(!tokens_match("s3cret", "s3cre"));
        assert!(!tokens_match("", "s3cret"));
        assert!(tokens_match("", ""));
    }
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

async fn list_libraries<R: OpenCodeRuntime>(
    State(service): State<AppService<R>>,
) -> Result<Json<Vec<Library>>, AppError> {
    Ok(Json(service.list_libraries()?))
}

/// Stream one content library out as a snapshot archive (gzip tar). The
/// selector may be a library id or, if unique, a name — exactly like
/// `noema-cli export`.
async fn export_library<R: OpenCodeRuntime>(
    State(service): State<AppService<R>>,
    Path(library_id): Path<String>,
) -> Result<Response, AppError> {
    let data_dir = service.data_dir().to_path_buf();
    let (library, temp) = tokio::task::spawn_blocking(move || {
        let temp = tempfile::NamedTempFile::new()?;
        let library = snapshot::export_library(&data_dir, &library_id, temp.path())?;
        Ok::<_, AppError>((library, temp))
    })
    .await
    .map_err(|error| AppError::Storage(format!("export task aborted: {error}")))??;
    let (archive, temp_path) = temp.into_parts();
    let length = archive.metadata()?.len();
    let body = SnapshotBody {
        chunks: ReaderStream::new(tokio::fs::File::from_std(archive)),
        // Keeps the temporary archive alive until the response body is fully
        // sent (or the connection drops); its Drop impl deletes the file.
        _temp: temp_path,
    };
    Ok(Response::builder()
        .header(header::CONTENT_TYPE, "application/gzip")
        .header(header::CONTENT_LENGTH, length)
        .header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{}.tar.gz\"", library.id),
        )
        .body(Body::from_stream(body))?)
}

/// Optional import query parameters: the archive itself is the request body;
/// the snapshot manifest supplies defaults for both when omitted.
#[derive(Deserialize)]
struct ImportQuery {
    name: Option<String>,
    description: Option<String>,
}

/// Import a snapshot archive (request body) as a brand-new, fully isolated
/// library. Same semantics as `noema-cli import`: always a fresh library,
/// full rollback on failure, hostile archives rejected.
async fn import_library<R: OpenCodeRuntime>(
    State(service): State<AppService<R>>,
    Query(query): Query<ImportQuery>,
    request: Request,
) -> Result<(StatusCode, Json<Library>), AppError> {
    // Stream the upload into a scratch archive on disk — no in-memory
    // buffering; the NamedTempFile is deleted on drop.
    let temp = tempfile::NamedTempFile::new()?;
    let mut reader = StreamReader::new(
        request
            .into_body()
            .into_data_stream()
            .map_err(io::Error::other),
    );
    let mut file = tokio::fs::File::from_std(temp.as_file().try_clone()?);
    tokio::io::copy(&mut reader, &mut file)
        .await
        .map_err(|error| AppError::BadRequest(format!("failed reading the upload: {error}")))?;
    drop(file);

    let data_dir = service.data_dir().to_path_buf();
    let imported = tokio::task::spawn_blocking(move || {
        snapshot::import_library(
            temp.path(),
            query.name.as_deref(),
            query.description.as_deref(),
            &data_dir,
        )
    })
    .await
    .map_err(|error| AppError::Storage(format!("import task aborted: {error}")))??;
    Ok((StatusCode::CREATED, Json(imported)))
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

/// Serve one knowledge file (`raw/` or `wiki/`) for client-side rendering —
/// e.g. a regulation page highlighting a verified citation's `[start, end)`
/// span. Prefix, traversal and symlink containment are enforced by the
/// service layer; anything outside the tree is a uniform 404.
async fn knowledge_file<R: OpenCodeRuntime>(
    State(service): State<AppService<R>>,
    Path((library_id, path)): Path<(String, String)>,
) -> Result<Response, AppError> {
    let resolved = service.knowledge_file(&library_id, &path)?;
    let metadata = tokio::fs::metadata(&resolved).await?;
    let content_type = match resolved.extension().and_then(|value| value.to_str()) {
        Some("md") => "text/markdown; charset=utf-8",
        _ => "text/plain; charset=utf-8",
    };
    let file = tokio::fs::File::open(&resolved).await?;
    Ok(Response::builder()
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CONTENT_LENGTH, metadata.len())
        // RFC 7231 IMF-fixdate, e.g. `Sun, 06 Nov 1994 08:49:37 GMT`.
        .header(
            header::LAST_MODIFIED,
            httpdate::fmt_http_date(metadata.modified()?),
        )
        .body(Body::from_stream(ReaderStream::new(file)))?)
}

/// Streams the temporary export archive to the client; dropping the body
/// (response complete or connection gone) deletes the file.
struct SnapshotBody {
    chunks: ReaderStream<tokio::fs::File>,
    _temp: tempfile::TempPath,
}

impl Stream for SnapshotBody {
    type Item = io::Result<Bytes>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.get_mut().chunks).poll_next(cx)
    }
}
