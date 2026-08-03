//! Coverage of the optional bearer-token guard: with a token configured,
//! every route but `/v1/health` requires the right `Authorization` header;
//! without one, the API stays fully open (the loopback default).

use std::sync::Arc;

use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
};
use noema::{
    AppError, AppService, http as http_api,
    runtime::{AgentRunRequest, AgentRunResult, OpenCodeRuntime},
};
use tempfile::TempDir;
use tower::ServiceExt;

mod common;

/// Runtime that is never exercised: auth is rejected before any handler.
struct IdleRuntime;

impl OpenCodeRuntime for IdleRuntime {
    async fn run_new_session(&self, _request: AgentRunRequest) -> Result<AgentRunResult, AppError> {
        Err(AppError::Runtime("not used by auth tests".into()))
    }
}

/// Router plus the TempDir backing its storage — keep both alive together.
fn fixture(auth_token: Option<&str>) -> (Router, TempDir) {
    let tempdir = tempfile::tempdir().unwrap();
    let service = AppService::with_runtime(
        common::config(tempdir.path().join("data"), auth_token),
        Arc::new(IdleRuntime),
    )
    .unwrap();
    (http_api::router(service), tempdir)
}

async fn status_of(router: Router, method: &str, uri: &str, bearer: Option<&str>) -> StatusCode {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(token) = bearer {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    router
        .oneshot(builder.body(Body::empty()).unwrap())
        .await
        .unwrap()
        .status()
}

#[tokio::test]
async fn open_by_default() {
    let (router, _tempdir) = fixture(None);
    assert_eq!(
        status_of(router.clone(), "GET", "/v1/libraries", None).await,
        StatusCode::OK
    );
    assert_eq!(
        status_of(router, "GET", "/v1/health", None).await,
        StatusCode::OK
    );
}

#[tokio::test]
async fn token_required_when_configured() {
    let (router, _tempdir) = fixture(Some("s3cret"));
    assert_eq!(
        status_of(router.clone(), "GET", "/v1/libraries", None).await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        status_of(router.clone(), "GET", "/v1/libraries", Some("wrong")).await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        status_of(router.clone(), "GET", "/v1/libraries", Some("s3cret")).await,
        StatusCode::OK
    );
    // Non-bearer schemes are rejected too.
    let request = Request::builder()
        .uri("/v1/libraries")
        .header(header::AUTHORIZATION, "Basic czNjcmV0")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        router.oneshot(request).await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn health_stays_open_for_probes() {
    let (router, _tempdir) = fixture(Some("s3cret"));
    assert_eq!(
        status_of(router, "GET", "/v1/health", None).await,
        StatusCode::OK
    );
}

#[tokio::test]
async fn health_withholds_server_internals_from_unauthenticated_probes() {
    let (router, _tempdir) = fixture(Some("s3cret"));
    let probe = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(probe.status(), StatusCode::OK);
    let body = to_bytes(probe.into_body(), usize::MAX).await.unwrap();
    let health: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(health["status"], "ok");
    assert!(health.get("data_dir").is_none(), "{health}");

    // An authorized probe (the CLI's `status` command) sees the details.
    let authorized = router
        .oneshot(
            Request::builder()
                .uri("/v1/health")
                .header(header::AUTHORIZATION, "Bearer s3cret")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = to_bytes(authorized.into_body(), usize::MAX).await.unwrap();
    let health: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(health["data_dir"].is_string(), "{health}");
}

#[tokio::test]
async fn mcp_endpoint_is_guarded() {
    let (router, _tempdir) = fixture(Some("s3cret"));
    assert_eq!(
        status_of(router, "POST", "/mcp", None).await,
        StatusCode::UNAUTHORIZED
    );
}
