use std::{
    fs,
    net::SocketAddr,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
};
use noema::{
    AppError, AppService, Config, http as http_api,
    models::{CreateLibraryRequest, DocumentInput, JobState},
    runtime::{AgentRunRequest, AgentRunResult, OpenCodeRuntime},
};
use tempfile::TempDir;
use tokio::time::sleep;
use tower::ServiceExt;

#[derive(Default)]
struct FakeRuntime {
    next_session: AtomicUsize,
    requests: Mutex<Vec<AgentRunRequest>>,
}

impl OpenCodeRuntime for FakeRuntime {
    async fn run_new_session(&self, request: AgentRunRequest) -> Result<AgentRunResult, AppError> {
        let number = self.next_session.fetch_add(1, Ordering::SeqCst) + 1;
        let session_id = format!("fake-session-{number}");
        let is_ingest = request.title.contains("ingestion");
        self.requests.lock().unwrap().push(request.clone());

        if is_ingest {
            let source = fs::read_dir(request.workdir.join("raw"))
                .unwrap()
                .next()
                .unwrap()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .to_string();
            let node = format!(
                "---\nnode_id: session-context\ncanonical_name: Session Context\nkind: concept\nsources:\n  - path: raw/{source}\nrelations:\n  depends_on: []\n  related_to: []\n  opposite_to: []\nclaim_type: observed\nconfidence: 1.0\ncreated_at: 2026-08-01T00:00:00Z\nupdated_at: 2026-08-01T00:00:00Z\n---\n\n# Session Context\n\nA test knowledge node.\n\n## Evidence\n\n- raw/{source}\n"
            );
            fs::write(request.workdir.join("wiki/session-context.md"), node).unwrap();
            fs::write(
                request.workdir.join("graphify-out/graph.json"),
                r#"{"nodes":[{"id":"session-context"}],"edges":[]}"#,
            )
            .unwrap();
        }

        let answer = if is_ingest {
            "摄入完成".into()
        } else {
            let source = fs::read_dir(request.workdir.join("raw"))
                .unwrap()
                .next()
                .unwrap()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .to_string();
            format!("答案见 `raw/{source}` 和 `wiki/session-context.md`。")
        };

        Ok(AgentRunResult {
            session_id,
            answer,
            tool_events: Vec::new(),
        })
    }
}

fn config(data_dir: PathBuf) -> Config {
    Config {
        data_dir,
        bind: "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
        opencode_url: "http://127.0.0.1:4096".into(),
        opencode_model: "opencode/deepseek-v4-flash-free".into(),
        opencode_timeout_secs: 5,
        transcript: false,
    }
}

async fn service_fixture() -> (TempDir, AppService<FakeRuntime>, Arc<FakeRuntime>) {
    let tempdir = tempfile::tempdir().unwrap();
    let runtime = Arc::new(FakeRuntime::default());
    let service =
        AppService::with_runtime(config(tempdir.path().join("data")), runtime.clone()).unwrap();
    (tempdir, service, runtime)
}

async fn wait_for_completion(
    service: &AppService<FakeRuntime>,
    library_id: &str,
    job_id: &str,
) -> noema::models::JobStatus {
    for _ in 0..100 {
        let status = service.job_status(library_id, job_id).unwrap();
        if matches!(status.status, JobState::Completed | JobState::Failed) {
            return status;
        }
        sleep(Duration::from_millis(10)).await;
    }
    panic!("ingestion job did not finish: {job_id}");
}

#[tokio::test]
async fn library_ingestion_query_and_session_isolation_work() {
    let (_tempdir, service, runtime) = service_fixture().await;
    let library = service
        .create_library(CreateLibraryRequest {
            name: "产品知识库".into(),
            description: Some("测试库".into()),
        })
        .await
        .unwrap();
    let root = PathBuf::from(&library.root);
    assert!(root.join("purpose.md").is_file());
    assert!(root.join("schema.md").is_file());
    assert!(root.join("index.md").is_file());
    assert!(root.join(".graphifyignore").is_file());
    assert!(root.join(".opencode/skills/kb-query/SKILL.md").is_file());

    let submitted = service
        .submit_document(
            &library.id,
            DocumentInput {
                filename: "source.md".into(),
                content: "# Session Context\n\nTest source.".into(),
                title: Some("来源文档".into()),
            },
        )
        .await
        .unwrap();
    assert!(!submitted.duplicate);
    let status = wait_for_completion(&service, &library.id, &submitted.job_id).await;
    assert_eq!(status.status, JobState::Completed, "{status:?}");
    assert!(status.session_id.is_some());
    assert!(root.join("raw").read_dir().unwrap().next().is_some());
    assert!(root.join("wiki/session-context.md").is_file());

    let second = service
        .submit_document(
            &library.id,
            DocumentInput {
                filename: "second.md".into(),
                content: "# Second Context\n\nA second source for incremental graph updates."
                    .into(),
                title: None,
            },
        )
        .await
        .unwrap();
    let second_status = wait_for_completion(&service, &library.id, &second.job_id).await;
    assert_eq!(
        second_status.status,
        JobState::Completed,
        "{second_status:?}"
    );

    let duplicate = service
        .submit_document(
            &library.id,
            DocumentInput {
                filename: "other.txt".into(),
                content: "# Session Context\n\nTest source.".into(),
                title: None,
            },
        )
        .await
        .unwrap();
    assert!(duplicate.duplicate);
    assert_eq!(
        service
            .job_status(&library.id, &duplicate.job_id)
            .unwrap()
            .status,
        JobState::Skipped
    );

    let first = service
        .query(&library.id, "这个概念是什么？")
        .await
        .unwrap();
    let second = service
        .query(&library.id, "它的来源是什么？")
        .await
        .unwrap();
    assert_ne!(first.session_id, second.session_id);
    assert!(!first.references.is_empty());
    let requests = runtime.requests.lock().unwrap();
    let ingestion_prompts = requests
        .iter()
        .filter(|request| request.title.contains("ingestion"))
        .map(|request| request.prompt.as_str())
        .collect::<Vec<_>>();
    assert_eq!(ingestion_prompts.len(), 2);
    assert!(ingestion_prompts[0].contains("/graphify .` 完整首次建图流程"));
    assert!(ingestion_prompts[1].contains("/graphify . --update"));
    assert!(requests.len() >= 4);
}

#[tokio::test]
async fn libraries_are_isolated_and_invalid_documents_are_rejected() {
    let (_tempdir, service, _runtime) = service_fixture().await;
    let first = service
        .create_library(CreateLibraryRequest {
            name: "库 A".into(),
            description: None,
        })
        .await
        .unwrap();
    let second = service
        .create_library(CreateLibraryRequest {
            name: "库 B".into(),
            description: None,
        })
        .await
        .unwrap();
    assert_ne!(first.id, second.id);
    assert_ne!(first.root, second.root);

    let invalid = service
        .submit_document(
            &first.id,
            DocumentInput {
                filename: "../escape.md".into(),
                content: "should fail".into(),
                title: None,
            },
        )
        .await;
    assert!(matches!(invalid, Err(AppError::BadRequest(_))));
    assert!(service.job_status(&first.id, "missing").is_err());
}

#[tokio::test]
async fn libraries_are_addressable_by_unique_name() {
    let (_tempdir, service, _runtime) = service_fixture().await;
    let library = service
        .create_library(CreateLibraryRequest {
            name: "法规库".into(),
            description: None,
        })
        .await
        .unwrap();

    let submitted = service
        .submit_document(
            "法规库",
            DocumentInput {
                filename: "source.md".into(),
                content: "# Session Context\n\nTest source.".into(),
                title: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(submitted.library_id, library.id);
    let status = wait_for_completion(&service, "法规库", &submitted.job_id).await;
    assert_eq!(status.status, JobState::Completed, "{status:?}");
    let answer = service.query("法规库", "这个概念是什么？").await.unwrap();
    assert_eq!(answer.library_id, library.id);

    // A duplicated name makes the selector ambiguous: callers must fall
    // back to the id, which always keeps working.
    service
        .create_library(CreateLibraryRequest {
            name: "法规库".into(),
            description: None,
        })
        .await
        .unwrap();
    let ambiguous = service
        .submit_document(
            "法规库",
            DocumentInput {
                filename: "other.md".into(),
                content: "content".into(),
                title: None,
            },
        )
        .await;
    assert!(matches!(ambiguous, Err(AppError::BadRequest(_))));
    assert!(service.job_status(&library.id, &submitted.job_id).is_ok());
    assert!(matches!(
        service.job_status("no-such-library", "missing"),
        Err(AppError::LibraryNotFound(_))
    ));
}

#[tokio::test]
async fn http_and_streamable_http_mcp_are_mounted() {
    let (_tempdir, service, _runtime) = service_fixture().await;
    let app = http_api::router(service);

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let health: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(health["status"], "ok");

    let initialize = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": "noema-test", "version": "0.1"}
        }
    });
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/mcp")
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::ACCEPT, "application/json, text/event-stream")
                .header(header::HOST, "localhost")
                .body(Body::from(initialize.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    if response.status() != StatusCode::OK {
        let status = response.status();
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        panic!(
            "MCP initialize failed with {status}: {}",
            String::from_utf8_lossy(&body)
        );
    }
    let content_type = response.headers().get(header::CONTENT_TYPE).unwrap();
    let content_type = content_type.to_str().unwrap();
    assert!(
        content_type.starts_with("application/json")
            || content_type.starts_with("text/event-stream")
    );
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let body = String::from_utf8_lossy(&body);
    assert!(body.contains("\"id\":1"));
    assert!(body.contains("\"capabilities\""));
    assert!(body.contains("\"tools\""));
}
