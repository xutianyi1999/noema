use std::{
    io,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use axum::{
    Json, Router,
    body::Body,
    extract::{DefaultBodyLimit, Path, Query, Request, State},
    http::{HeaderMap, StatusCode, header},
    middleware,
    response::Response,
    routing::{get, post},
};
use bytes::Bytes;
use futures_util::{Stream, TryStreamExt};
use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use serde::Deserialize;
use subtle::ConstantTimeEq;
use tokio::io::AsyncReadExt;
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
            // axum's default JSON body limit is 2 MiB — below the size of
            // real regulatory texts. Snapshots keep their own 512 MB cap.
            post(submit_document::<R>).layer(DefaultBodyLimit::max(MAX_JSON_BODY)),
        )
        .route(
            "/v1/libraries/{library_id}/jobs/{job_id}",
            get(job_status::<R>),
        )
        .route(
            "/v1/libraries/{library_id}/query",
            post(query::<R>).layer(DefaultBodyLimit::max(MAX_JSON_BODY)),
        )
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
    match bearer_token(request.headers()) {
        Some(candidate) if tokens_match(candidate, &token) => Ok(next.run(request).await),
        _ => Err(AppError::Unauthorized),
    }
}

/// The token of an `Authorization: Bearer <token>` header, when present,
/// well-formed UTF-8, and actually bearer-shaped.
fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
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

async fn health<R: OpenCodeRuntime>(
    State(service): State<AppService<R>>,
    headers: HeaderMap,
) -> Json<HealthResponse> {
    Json(service.health(probe_carries_the_token(&service, &headers)))
}

/// The health probe stays open to unauthenticated orchestrators; server
/// internals (data directory, OpenCode URL) only travel with it when the API
/// itself runs open or the probe carries the configured token.
fn probe_carries_the_token<R: OpenCodeRuntime>(
    service: &AppService<R>,
    headers: &HeaderMap,
) -> bool {
    match service.auth_token() {
        None => true,
        Some(expected) => {
            bearer_token(headers).is_some_and(|candidate| tokens_match(candidate, expected))
        }
    }
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
            // The plain `filename` parameter is ASCII-only by RFC 6266, so
            // CJK library ids travel percent-encoded in `filename*` (RFC
            // 5987); modern clients prefer it, legacy ones still get a
            // syntactically valid quoted-string.
            format!(
                "attachment; filename=\"{id}.tar.gz\"; filename*=UTF-8''{encoded}.tar.gz",
                id = library.id,
                encoded = utf8_percent_encode(&library.id, NON_ALPHANUMERIC)
            ),
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

/// Upper bound for one snapshot upload. Snapshots are libraries of plain
/// text, so this only fences off abuse (disk-fill uploads); real imports
/// stay far below it. Decompression is bounded separately inside
/// [`crate::snapshot`] (gzip bombs compress small).
const MAX_IMPORT_UPLOAD: u64 = 512 * 1024 * 1024;

/// Upper bound for one JSON body (document submission, query prompt).
/// Documents are plain text, but multi-megabyte regulations are routine;
/// axum's 2 MiB default rejected them.
const MAX_JSON_BODY: usize = 64 * 1024 * 1024;

/// Import a snapshot archive (request body) as a brand-new, fully isolated
/// library. Same semantics as `noema-cli import`: always a fresh library,
/// full rollback on failure, hostile archives rejected.
async fn import_library<R: OpenCodeRuntime>(
    State(service): State<AppService<R>>,
    Query(query): Query<ImportQuery>,
    request: Request,
) -> Result<(StatusCode, Json<Library>), AppError> {
    // Stream the upload into a scratch archive on disk — no in-memory
    // buffering; the NamedTempFile is deleted on drop. The `.take` ceiling
    // bounds disk fill; one extra byte distinguishes "at the limit" from
    // "over the limit".
    let temp = tempfile::NamedTempFile::new()?;
    let mut reader = StreamReader::new(
        request
            .into_body()
            .into_data_stream()
            .map_err(io::Error::other),
    )
    .take(MAX_IMPORT_UPLOAD + 1);
    let mut file = tokio::fs::File::from_std(temp.as_file().try_clone()?);
    tokio::io::copy(&mut reader, &mut file)
        .await
        .map_err(|error| AppError::BadRequest(format!("failed reading the upload: {error}")))?;
    drop(file);
    if temp.as_file().metadata()?.len() > MAX_IMPORT_UPLOAD {
        return Err(AppError::BadRequest(format!(
            "snapshot upload exceeds the {MAX_IMPORT_UPLOAD}-byte limit"
        )));
    }

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
    // The file passed containment a moment ago; anything failing now (e.g.
    // deleted mid-request) is the same 404 as any absent path — and never an
    // error body carrying the resolved absolute path.
    let not_found = || AppError::FileNotFound(path.clone());
    // Open first, then stat the open handle: metadata captured before the
    // open could describe a version a concurrent promotion already
    // replaced, advertising a Content-Length the body disagrees with.
    let file = tokio::fs::File::open(&resolved)
        .await
        .map_err(|_| not_found())?;
    let metadata = file.metadata().await.map_err(|_| not_found())?;
    let content_type = match resolved.extension().and_then(|value| value.to_str()) {
        Some("md") => "text/markdown; charset=utf-8",
        _ => "text/plain; charset=utf-8",
    };
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
