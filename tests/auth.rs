//! Coverage of the optional bearer-token guard: with a token configured,
//! every route but `/v1/health` requires the right `Authorization` header;
//! without one, the API stays fully open (the loopback default).

use std::{net::SocketAddr, path::PathBuf, sync::Arc};

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode, header},
};
use noema::{
    AppError, AppService, Config, http as http_api,
    runtime::{AgentRunRequest, AgentRunResult, OpenCodeRuntime},
};
use tempfile::TempDir;
use tower::ServiceExt;

/// Runtime that is never exercised: auth is rejected before any handler.
struct IdleRuntime;

impl OpenCodeRuntime for IdleRuntime {
    async fn run_new_session(&self, _request: AgentRunRequest) -> Result<AgentRunResult, AppError> {
        Err(AppError::Runtime("not used by auth tests".into()))
    }
}

fn config(data_dir: PathBuf, auth_token: Option<&str>) -> Config {
    Config {
        data_dir,
        bind: "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
        opencode_url: "http://127.0.0.1:4096".into(),
        opencode_model: "opencode/deepseek-v4-flash-free".into(),
        opencode_timeout_secs: 5,
        transcript: false,
        max_sessions: 4,
        auth_token: auth_token.map(str::to_string),
    }
}

/// Router plus the TempDir backing its storage — keep both alive together.
fn fixture(auth_token: Option<&str>) -> (Router, TempDir) {
    let tempdir = tempfile::tempdir().unwrap();
    let service = AppService::with_runtime(
        config(tempdir.path().join("data"), auth_token),
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
async fn mcp_endpoint_is_guarded() {
    let (router, _tempdir) = fixture(Some("s3cret"));
    assert_eq!(
        status_of(router, "POST", "/mcp", None).await,
        StatusCode::UNAUTHORIZED
    );
}
