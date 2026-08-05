use std::{
    fs,
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
    AppError, AppService, http as http_api,
    models::{CreateLibraryRequest, DocumentInput, JobState, Library},
    runtime::{AgentRunRequest, AgentRunResult, OpenCodeRuntime},
};
use tempfile::TempDir;
use tokio::time::sleep;
use tower::ServiceExt;

mod common;

/// A test hook fired at the start of every session, letting a test perturb
/// the library mid-run (e.g. simulate a concurrent submission).
type SessionHook = Box<dyn Fn(&AgentRunRequest) + Send + Sync>;

#[derive(Default)]
struct FakeRuntime {
    next_session: AtomicUsize,
    requests: Mutex<Vec<AgentRunRequest>>,
    query_session_ids: Mutex<Vec<Option<String>>>,
    /// Remaining ingest sessions to fail with a runtime error. Only ingest
    /// sessions consume the counter; queries pass through untouched.
    fail_next_ingests: AtomicUsize,
    /// Fired at the start of every session, ingest and query alike, with
    /// the request about to run; callbacks filter on the title themselves.
    on_session: Mutex<Option<SessionHook>>,
}

impl FakeRuntime {
    async fn run(
        &self,
        request: AgentRunRequest,
        requested_session_id: Option<&str>,
    ) -> Result<AgentRunResult, AppError> {
        if let Some(hook) = self.on_session.lock().unwrap().as_ref() {
            hook(&request);
        }
        let is_ingest = request.title.contains("ingestion");
        let session_id = requested_session_id.map(str::to_string).unwrap_or_else(|| {
            let number = self.next_session.fetch_add(1, Ordering::SeqCst) + 1;
            format!("fake-session-{number}")
        });
        if !is_ingest {
            self.query_session_ids
                .lock()
                .unwrap()
                .push(requested_session_id.map(str::to_string));
        }
        self.requests.lock().unwrap().push(request.clone());
        // Failure injection: consume one pending failure per ingest session,
        // after recording the request and before writing any artifact.
        if is_ingest {
            let should_fail = self
                .fail_next_ingests
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |pending| {
                    pending.checked_sub(1)
                })
                .is_ok();
            if should_fail {
                return Err(AppError::Runtime("injected ingestion failure".into()));
            }
        }
        let mut sources = fs::read_dir(request.workdir.join("raw"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        sources.sort();
        let source = sources[0].clone();

        if is_ingest {
            // Every raw/ document becomes a source of the one node, so a
            // completed ingest deterministically compiles the whole library
            // regardless of directory iteration order.
            let mut node_sources = String::new();
            let mut evidence = String::new();
            for name in &sources {
                node_sources.push_str(&format!("  - path: raw/{name}\n"));
                evidence.push_str(&format!("- raw/{name}\n"));
            }
            let node = format!(
                "---\nnode_id: session-context\ncanonical_name: Session Context\nkind: concept\nsources:\n{node_sources}relations:\n  depends_on: []\n  related_to: []\n  opposite_to: []\nclaim_type: observed\nconfidence: 1.0\ncreated_at: 2026-08-01T00:00:00Z\nupdated_at: 2026-08-01T00:00:00Z\n---\n\n# Session Context\n\nA test knowledge node.\n\n## Evidence\n\n{evidence}"
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
            let content = fs::read_to_string(request.workdir.join("raw").join(&source)).unwrap();
            let good = serde_json::to_string(content.lines().next().unwrap_or_default()).unwrap();
            if request.prompt.contains("bad-contract") {
                format!("答案见 `raw/{source}`，这不是契约 JSON。")
            } else if request.prompt.contains("bad-quote") {
                // First citation's quote is not verbatim in the source and
                // must be dropped along with its [1] marker; the second is
                // honest and survives.
                format!(
                    "{{\"answer\": \"错误结论[1]。正确结论[2]。\", \"references\": [{{\"source\": \"raw/{source}\", \"quote\": \"文件中不存在的引文\"}}, {{\"source\": \"raw/{source}\", \"quote\": {good}, \"locator\": \"第一条\"}}]}}"
                )
            } else {
                format!(
                    "{{\"answer\": \"答案依据原文[1]。\", \"references\": [{{\"source\": \"raw/{source}\", \"quote\": {good}, \"locator\": \"第一条\"}}]}}"
                )
            }
        };

        Ok(AgentRunResult {
            session_id,
            answer,
            tool_events: Vec::new(),
        })
    }
}

impl OpenCodeRuntime for FakeRuntime {
    async fn run_new_session(&self, request: AgentRunRequest) -> Result<AgentRunResult, AppError> {
        self.run(request, None).await
    }

    async fn run_query_session(
        &self,
        request: AgentRunRequest,
        session_id: Option<&str>,
    ) -> Result<AgentRunResult, AppError> {
        self.run(request, session_id).await
    }
}

async fn service_fixture() -> (TempDir, AppService<FakeRuntime>, Arc<FakeRuntime>) {
    let tempdir = tempfile::tempdir().unwrap();
    let runtime = Arc::new(FakeRuntime::default());
    let service = AppService::with_runtime(
        common::config(tempdir.path().join("data"), None),
        runtime.clone(),
    )
    .unwrap();
    (tempdir, service, runtime)
}

async fn wait_for_completion(
    service: &AppService<FakeRuntime>,
    library_id: &str,
    job_id: &str,
) -> noema::models::JobStatus {
    for _ in 0..500 {
        let status = service.job_status(library_id, job_id).unwrap();
        if matches!(
            status.status,
            JobState::Completed | JobState::Failed | JobState::Skipped
        ) {
            return status;
        }
        sleep(Duration::from_millis(10)).await;
    }
    panic!("ingestion job did not finish: {job_id}");
}

/// Standard opening for most tests: a fresh library whose one source
/// document is already compiled.
async fn library_with_source(
    service: &AppService<FakeRuntime>,
    name: &str,
    title: Option<&str>,
) -> Library {
    let library = service
        .create_library(CreateLibraryRequest {
            name: name.into(),
            description: None,
        })
        .await
        .unwrap();
    let submitted = service
        .submit_documents(
            &library.id,
            vec![DocumentInput {
                filename: "source.md".into(),
                content: "# Session Context\n\nTest source.".into(),
                title: title.map(str::to_string),
            }],
        )
        .await
        .unwrap();
    let status = wait_for_completion(service, &library.id, &submitted.job_id).await;
    assert_eq!(status.status, JobState::Completed, "{status:?}");
    library
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
        .submit_documents(
            &library.id,
            vec![DocumentInput {
                filename: "source.md".into(),
                content: "# Session Context\n\nTest source.".into(),
                title: Some("来源文档".into()),
            }],
        )
        .await
        .unwrap();
    assert!(!submitted.documents[0].duplicate);
    let status = wait_for_completion(&service, &library.id, &submitted.job_id).await;
    assert_eq!(status.status, JobState::Completed, "{status:?}");
    assert!(status.session_id.is_some());
    // Documents keep their original names: no hash prefix, no rewrite.
    assert!(root.join("raw/source.md").is_file());
    assert!(root.join("wiki/session-context.md").is_file());

    let second = service
        .submit_documents(
            &library.id,
            vec![DocumentInput {
                filename: "second.md".into(),
                content: "# Second Context\n\nA second source for incremental graph updates."
                    .into(),
                title: None,
            }],
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
        .submit_documents(
            &library.id,
            vec![DocumentInput {
                filename: "other.txt".into(),
                content: "# Session Context\n\nTest source.".into(),
                title: None,
            }],
        )
        .await
        .unwrap();
    assert!(duplicate.documents[0].duplicate && duplicate.documents[0].skipped);
    assert_eq!(
        service
            .job_status(&library.id, &duplicate.job_id)
            .unwrap()
            .status,
        JobState::Skipped
    );

    let first = service
        .query(&library.id, "这个概念是什么？", None)
        .await
        .unwrap();
    let second = service
        .query(&library.id, "它的来源是什么？", None)
        .await
        .unwrap();
    assert_ne!(first.session_id, second.session_id);
    let continued = service
        .query(&library.id, "继续这个话题", Some(&first.session_id))
        .await
        .unwrap();
    assert_eq!(continued.session_id, first.session_id);
    assert_eq!(
        runtime.query_session_ids.lock().unwrap().as_slice(),
        &[None, None, Some(first.session_id.clone())]
    );
    let other_library = service
        .create_library(CreateLibraryRequest {
            name: "另一内容库".into(),
            description: None,
        })
        .await
        .unwrap();
    let error = service
        .query(&other_library.id, "越库复用", Some(&first.session_id))
        .await
        .unwrap_err();
    assert!(matches!(error, AppError::BadRequest(_)));
    let error = service
        .query(&library.id, "空会话", Some("   "))
        .await
        .unwrap_err();
    assert!(matches!(error, AppError::BadRequest(_)));
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
    assert!(requests.len() >= 5);
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
        .submit_documents(
            &first.id,
            vec![DocumentInput {
                filename: "../escape.md".into(),
                content: "should fail".into(),
                title: None,
            }],
        )
        .await;
    assert!(matches!(invalid, Err(AppError::BadRequest(_))));
    assert!(service.job_status(&first.id, "missing").is_err());

    // Filenames are stable identities: the same name with different content
    // is rejected instead of coexisting under a hash prefix.
    service
        .submit_documents(
            &first.id,
            vec![DocumentInput {
                filename: "note.md".into(),
                content: "first version".into(),
                title: None,
            }],
        )
        .await
        .unwrap();
    let conflict = service
        .submit_documents(
            &first.id,
            vec![DocumentInput {
                filename: "note.md".into(),
                content: "second, different version".into(),
                title: None,
            }],
        )
        .await;
    assert!(matches!(conflict, Err(AppError::Conflict(_))));
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
    // The name is the id and the directory name, verbatim.
    assert_eq!(library.id, "法规库");
    assert!(PathBuf::from(&library.root).ends_with("libraries/法规库"));

    let submitted = service
        .submit_documents(
            "法规库",
            vec![DocumentInput {
                filename: "source.md".into(),
                content: "# Session Context\n\nTest source.".into(),
                title: None,
            }],
        )
        .await
        .unwrap();
    assert_eq!(submitted.library_id, library.id);
    let status = wait_for_completion(&service, "法规库", &submitted.job_id).await;
    assert_eq!(status.status, JobState::Completed, "{status:?}");
    let answer = service
        .query("法规库", "这个概念是什么？", None)
        .await
        .unwrap();
    assert_eq!(answer.library_id, library.id);

    // Names are unique identities now: a second library under the same name
    // is rejected instead of making the selector ambiguous.
    let duplicate = service
        .create_library(CreateLibraryRequest {
            name: "法规库".into(),
            description: None,
        })
        .await;
    assert!(matches!(duplicate, Err(AppError::Conflict(_))));
    assert!(service.job_status(&library.id, &submitted.job_id).is_ok());
    assert!(matches!(
        service.job_status("no-such-library", "missing"),
        Err(AppError::LibraryNotFound(_))
    ));

    // NFC and NFD spellings of one name are one identity: a library created
    // with a decomposed name is addressable in composed form.
    let decomposed = service
        .create_library(CreateLibraryRequest {
            name: "cafe\u{301}".into(),
            description: None,
        })
        .await
        .unwrap();
    assert_eq!(decomposed.id, "caf\u{e9}");
    assert!(matches!(
        service.job_status("caf\u{e9}", "missing"),
        Err(AppError::JobNotFound(_))
    ));
}

#[tokio::test]
async fn concurrent_submissions_to_one_library_are_serialized() {
    let (_tempdir, service, runtime) = service_fixture().await;
    let library = service
        .create_library(CreateLibraryRequest {
            name: "并发库".into(),
            description: None,
        })
        .await
        .unwrap();

    // Two documents in without waiting: both jobs are accepted at once.
    let first = service
        .submit_documents(
            &library.id,
            vec![DocumentInput {
                filename: "one.md".into(),
                content: "# One\n\nFirst source.".into(),
                title: None,
            }],
        )
        .await
        .unwrap();
    let second = service
        .submit_documents(
            &library.id,
            vec![DocumentInput {
                filename: "two.md".into(),
                content: "# Two\n\nSecond source.".into(),
                title: None,
            }],
        )
        .await
        .unwrap();

    let first_status = wait_for_completion(&service, &library.id, &first.job_id).await;
    let second_status = wait_for_completion(&service, &library.id, &second.job_id).await;
    assert_eq!(first_status.status, JobState::Completed, "{first_status:?}");

    // The ingest lock serializes same-library ingests: the second job only
    // prepares its staging after the first promotion. Either it then ran on
    // the first job's graph (the incremental prompt), or the first session
    // already compiled its document (the fake agent references every raw/
    // file in its staging) and it skipped outright. Never two overlapping
    // promotions, never a second full build.
    let prompts: Vec<String> = runtime
        .requests
        .lock()
        .unwrap()
        .iter()
        .filter(|request| request.title.contains("ingestion"))
        .map(|request| request.prompt.clone())
        .collect();
    if second_status.status == JobState::Completed {
        assert_eq!(prompts.len(), 2, "{prompts:?}");
        assert!(prompts[0].contains("/graphify .` 完整首次建图流程"));
        assert!(prompts[1].contains("/graphify . --update"));
    } else {
        assert_eq!(second_status.status, JobState::Skipped, "{second_status:?}");
        assert_eq!(prompts.len(), 1, "{prompts:?}");
    }
    // Both documents are compiled either way.
    let root = service.storage.library_root(&library.id).unwrap();
    let node = fs::read_to_string(root.join("wiki/session-context.md")).unwrap();
    assert!(node.contains("raw/one.md"), "{node}");
    assert!(node.contains("raw/two.md"), "{node}");
}

#[tokio::test]
async fn citations_are_verified_and_unverified_markers_are_stripped() {
    let (_tempdir, service, _runtime) = service_fixture().await;
    let library = library_with_source(&service, "引文库", Some("来源文档")).await;

    // The first declared citation quotes text the source does not contain:
    // it is dropped and its [1] marker stripped; the honest second citation
    // keeps its id (no renumbering) and carries server-computed offsets.
    let answer = service
        .query(&library.id, "bad-quote 场景", None)
        .await
        .unwrap();
    assert_eq!(answer.answer, "错误结论。正确结论[2]。");
    assert_eq!(answer.references.len(), 1);
    let reference = &answer.references[0];
    assert_eq!(reference.id, 2);
    assert_eq!(reference.title, "来源文档");
    assert_eq!(reference.locator.as_deref(), Some("第一条"));
    assert_eq!(reference.quote.as_deref(), Some("# Session Context"));
    assert_eq!(reference.start, Some(0));
    assert_eq!(reference.end, Some(17));
    assert_eq!(reference.lines, Some((1, 1)));
}

#[tokio::test]
async fn non_contract_answer_degrades_to_plain_text_without_references() {
    let (_tempdir, service, _runtime) = service_fixture().await;
    let library = library_with_source(&service, "降级库", None).await;

    let answer = service
        .query(&library.id, "bad-contract 场景", None)
        .await
        .unwrap();
    assert!(
        answer.answer.contains("这不是契约 JSON"),
        "{}",
        answer.answer
    );
    assert!(
        answer.references.is_empty(),
        "unverified citations never reach the client"
    );
}

#[tokio::test]
async fn knowledge_files_are_served_with_safety_checks() {
    let (_tempdir, service, _runtime) = service_fixture().await;
    library_with_source(&service, "fileslib", None).await;
    let app = http_api::router(service);

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v1/libraries/fileslib/files/raw/source.md")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get(header::CONTENT_TYPE).unwrap(),
        "text/markdown; charset=utf-8"
    );
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    assert_eq!(body, "# Session Context\n\nTest source.");

    // Anything outside raw/|wiki/ and anything missing inside are the same
    // uniform 404 — the status code never reveals the path policy.
    for (uri, status) in [
        (
            "/v1/libraries/fileslib/files/library.sqlite",
            StatusCode::NOT_FOUND,
        ),
        (
            "/v1/libraries/fileslib/files/raw/absent.md",
            StatusCode::NOT_FOUND,
        ),
    ] {
        let response = app
            .clone()
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), status, "{uri}");
    }
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

#[tokio::test]
async fn a_submission_landing_mid_ingest_no_longer_fails_validation() {
    let (_tempdir, service, runtime) = service_fixture().await;
    let library = service
        .create_library(CreateLibraryRequest {
            name: "并发落盘库".into(),
            description: None,
        })
        .await
        .unwrap();
    let root = PathBuf::from(&library.root);
    // While this job's session runs, a document lands in the live raw/ —
    // exactly what a concurrent submission does. Validation must compare
    // the staging copy against its preparation baseline, not the live root.
    *runtime.on_session.lock().unwrap() = Some(Box::new(|request| {
        if !request.title.contains("ingestion") {
            return;
        }
        let late = request
            .workdir
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("raw/late.md");
        fs::write(late, "# Late arrival\n\nSubmitted mid-run.").unwrap();
    }));

    let submitted = service
        .submit_documents(
            &library.id,
            vec![DocumentInput {
                filename: "source.md".into(),
                content: "# Session Context\n\nTest source.".into(),
                title: None,
            }],
        )
        .await
        .unwrap();
    let status = wait_for_completion(&service, &library.id, &submitted.job_id).await;
    assert_eq!(status.status, JobState::Completed, "{status:?}");
    assert!(root.join("raw/late.md").is_file());
}

#[tokio::test]
async fn a_job_that_failed_before_promotion_is_recompiled_by_the_next_ingest() {
    let (_tempdir, service, runtime) = service_fixture().await;
    let library = service
        .create_library(CreateLibraryRequest {
            name: "补编译库".into(),
            description: None,
        })
        .await
        .unwrap();
    runtime.fail_next_ingests.store(1, Ordering::SeqCst);

    let first = service
        .submit_documents(
            &library.id,
            vec![DocumentInput {
                filename: "doc1.md".into(),
                content: "# One\n\nFirst source.".into(),
                title: None,
            }],
        )
        .await
        .unwrap();
    let first_status = wait_for_completion(&service, &library.id, &first.job_id).await;
    assert_eq!(first_status.status, JobState::Failed, "{first_status:?}");

    let second = service
        .submit_documents(
            &library.id,
            vec![DocumentInput {
                filename: "doc2.md".into(),
                content: "# Two\n\nSecond source.".into(),
                title: None,
            }],
        )
        .await
        .unwrap();
    let second_status = wait_for_completion(&service, &library.id, &second.job_id).await;
    assert_eq!(
        second_status.status,
        JobState::Completed,
        "{second_status:?}"
    );

    // The failed job left doc1 in raw/ without any node: the second job's
    // prompt names it alongside the new document so it gets compiled too.
    let prompts: Vec<String> = runtime
        .requests
        .lock()
        .unwrap()
        .iter()
        .filter(|request| request.title.contains("ingestion"))
        .map(|request| request.prompt.clone())
        .collect();
    assert_eq!(prompts.len(), 2);
    assert!(prompts[1].contains("raw/doc1.md"), "{}", prompts[1]);
    assert!(prompts[1].contains("raw/doc2.md"), "{}", prompts[1]);
}

#[tokio::test]
async fn a_document_compiled_by_a_predecessor_job_is_not_ingested_twice() {
    let (_tempdir, service, runtime) = service_fixture().await;
    let library = service
        .create_library(CreateLibraryRequest {
            name: "防重库".into(),
            description: None,
        })
        .await
        .unwrap();
    // job1 fails before promotion and leaves a.md in raw/ without a node.
    runtime.fail_next_ingests.store(1, Ordering::SeqCst);
    let first = service
        .submit_documents(
            &library.id,
            vec![DocumentInput {
                filename: "a.md".into(),
                content: "# A\n\nFirst source.".into(),
                title: None,
            }],
        )
        .await
        .unwrap();
    let first_status = wait_for_completion(&service, &library.id, &first.job_id).await;
    assert_eq!(first_status.status, JobState::Failed, "{first_status:?}");

    // job2 compiles both a.md (named as an uncompiled extra) and b.md — the
    // fake runtime references every raw/ document from its node.
    let second = service
        .submit_documents(
            &library.id,
            vec![DocumentInput {
                filename: "b.md".into(),
                content: "# B\n\nSecond source.".into(),
                title: None,
            }],
        )
        .await
        .unwrap();
    // b.md resubmitted while job2 may still hold the library lock: dedupe
    // sees no wiki node for it yet and accepts a second job. Once job2
    // promotes, that late job must notice b.md is compiled and skip instead
    // of running a redundant session. (If job2 already promoted before this
    // submission, the same skip happens synchronously — either way no third
    // session runs.)
    let third = service
        .submit_documents(
            &library.id,
            vec![DocumentInput {
                filename: "b.md".into(),
                content: "# B\n\nSecond source.".into(),
                title: None,
            }],
        )
        .await
        .unwrap();

    let second_status = wait_for_completion(&service, &library.id, &second.job_id).await;
    let third_status = wait_for_completion(&service, &library.id, &third.job_id).await;
    let statuses = [second_status.status, third_status.status];
    assert!(
        statuses.contains(&JobState::Completed) && statuses.contains(&JobState::Skipped),
        "{second_status:?} {third_status:?}"
    );
    // Exactly one session beyond the injected failure: the late job skipped.
    let ingest_sessions = runtime
        .requests
        .lock()
        .unwrap()
        .iter()
        .filter(|request| request.title.contains("ingestion"))
        .count();
    assert_eq!(
        ingest_sessions, 2,
        "no redundant session for an already-compiled document"
    );
}

#[tokio::test]
async fn startup_repairs_a_library_whose_bootstrap_was_interrupted() {
    let (tempdir, service, _runtime) = service_fixture().await;
    let library = service
        .create_library(CreateLibraryRequest {
            name: "自愈库".into(),
            description: None,
        })
        .await
        .unwrap();
    let marker = PathBuf::from(&library.root).join(".opencode/skills/graphify/SKILL.md");
    assert!(marker.is_file(), "installer completed at creation");
    // Simulate a creation interrupted before the installer finished.
    fs::remove_file(&marker).unwrap();

    // A fresh service over the same data directory runs startup maintenance
    // and re-runs the full bootstrap for the broken library.
    let _repaired = AppService::with_runtime(
        common::config(tempdir.path().join("data"), None),
        Arc::new(FakeRuntime::default()),
    )
    .unwrap();
    assert!(marker.is_file(), "startup re-ran the graphify installer");
}

#[tokio::test]
async fn a_batch_of_documents_is_compiled_in_one_ingestion_job() {
    let (_tempdir, service, runtime) = service_fixture().await;
    let library = service
        .create_library(CreateLibraryRequest {
            name: "批量摄入库".into(),
            description: None,
        })
        .await
        .unwrap();

    let submitted = service
        .submit_documents(
            &library.id,
            vec![
                DocumentInput {
                    filename: "a.md".into(),
                    content: "# A\n\nFirst source.".into(),
                    title: None,
                },
                DocumentInput {
                    filename: "b.md".into(),
                    content: "# B\n\nSecond source.".into(),
                    title: None,
                },
                DocumentInput {
                    filename: "c.md".into(),
                    content: "# C\n\nThird source.".into(),
                    title: None,
                },
            ],
        )
        .await
        .unwrap();
    assert_eq!(submitted.documents.len(), 3);
    for entry in &submitted.documents {
        assert!(!entry.duplicate, "{entry:?}");
        assert!(!entry.skipped, "{entry:?}");
    }
    let status = wait_for_completion(&service, &library.id, &submitted.job_id).await;
    assert_eq!(status.status, JobState::Completed, "{status:?}");

    // One batch = one staging workspace = one agent session naming every
    // document, with the cross-document merge clause.
    let prompts: Vec<String> = runtime
        .requests
        .lock()
        .unwrap()
        .iter()
        .filter(|request| request.title.contains("ingestion"))
        .map(|request| request.prompt.clone())
        .collect();
    assert_eq!(prompts.len(), 1, "{prompts:?}");
    assert!(prompts[0].contains("依次阅读以下源文档"), "{}", prompts[0]);
    for path in ["raw/a.md", "raw/b.md", "raw/c.md"] {
        assert!(prompts[0].contains(path), "{}", prompts[0]);
    }
    assert!(prompts[0].contains("合并为一个节点"), "{}", prompts[0]);

    let root = service.storage.library_root(&library.id).unwrap();
    let node = fs::read_to_string(root.join("wiki/session-context.md")).unwrap();
    for path in ["raw/a.md", "raw/b.md", "raw/c.md"] {
        assert!(node.contains(path), "{node}");
    }
}

#[tokio::test]
async fn a_batch_of_already_compiled_documents_is_a_noop_skip() {
    let (_tempdir, service, runtime) = service_fixture().await;
    let library = service
        .create_library(CreateLibraryRequest {
            name: "批量重提库".into(),
            description: None,
        })
        .await
        .unwrap();
    let batch = || {
        vec![
            DocumentInput {
                filename: "a.md".into(),
                content: "# A\n\nFirst source.".into(),
                title: None,
            },
            DocumentInput {
                filename: "b.md".into(),
                content: "# B\n\nSecond source.".into(),
                title: None,
            },
        ]
    };

    let first = service
        .submit_documents(&library.id, batch())
        .await
        .unwrap();
    let first_status = wait_for_completion(&service, &library.id, &first.job_id).await;
    assert_eq!(first_status.status, JobState::Completed, "{first_status:?}");

    // Resubmitting the identical batch stores nothing new and skips: every
    // entry is a genuine no-op, and no second session ever runs.
    let second = service
        .submit_documents(&library.id, batch())
        .await
        .unwrap();
    for entry in &second.documents {
        assert!(entry.duplicate, "{entry:?}");
        assert!(entry.skipped, "{entry:?}");
    }
    assert_eq!(
        service
            .job_status(&library.id, &second.job_id)
            .unwrap()
            .status,
        JobState::Skipped
    );
    let ingest_sessions = runtime
        .requests
        .lock()
        .unwrap()
        .iter()
        .filter(|request| request.title.contains("ingestion"))
        .count();
    assert_eq!(ingest_sessions, 1);
}

#[tokio::test]
async fn a_batch_mixing_compiled_and_fresh_documents_skips_only_the_compiled() {
    let (_tempdir, service, runtime) = service_fixture().await;
    let library = service
        .create_library(CreateLibraryRequest {
            name: "批量混合库".into(),
            description: None,
        })
        .await
        .unwrap();
    let alone = service
        .submit_documents(
            &library.id,
            vec![DocumentInput {
                filename: "a.md".into(),
                content: "# A\n\nFirst source.".into(),
                title: None,
            }],
        )
        .await
        .unwrap();
    let alone_status = wait_for_completion(&service, &library.id, &alone.job_id).await;
    assert_eq!(alone_status.status, JobState::Completed, "{alone_status:?}");

    let submitted = service
        .submit_documents(
            &library.id,
            vec![
                DocumentInput {
                    filename: "a.md".into(),
                    content: "# A\n\nFirst source.".into(),
                    title: None,
                },
                DocumentInput {
                    filename: "b.md".into(),
                    content: "# B\n\nSecond source.".into(),
                    title: None,
                },
            ],
        )
        .await
        .unwrap();
    assert!(
        submitted.documents[0].duplicate,
        "{:?}",
        submitted.documents
    );
    assert!(submitted.documents[0].skipped, "{:?}", submitted.documents);
    assert!(
        !submitted.documents[1].duplicate,
        "{:?}",
        submitted.documents
    );
    assert!(!submitted.documents[1].skipped, "{:?}", submitted.documents);
    let status = wait_for_completion(&service, &library.id, &submitted.job_id).await;
    assert_eq!(status.status, JobState::Completed, "{status:?}");

    // Only the fresh document needed a session: one earlier plus one now.
    let ingest_sessions = runtime
        .requests
        .lock()
        .unwrap()
        .iter()
        .filter(|request| request.title.contains("ingestion"))
        .count();
    assert_eq!(ingest_sessions, 2);
}

#[tokio::test]
async fn an_empty_batch_is_rejected() {
    let (_tempdir, service, _runtime) = service_fixture().await;
    let library = service
        .create_library(CreateLibraryRequest {
            name: "空批库".into(),
            description: None,
        })
        .await
        .unwrap();
    let result = service.submit_documents(&library.id, Vec::new()).await;
    assert!(matches!(result, Err(AppError::BadRequest(_))));
}

#[tokio::test]
async fn a_batch_with_an_empty_document_is_rejected_naming_the_filename() {
    let (_tempdir, service, _runtime) = service_fixture().await;
    let library = service
        .create_library(CreateLibraryRequest {
            name: "空文档批库".into(),
            description: None,
        })
        .await
        .unwrap();
    let result = service
        .submit_documents(
            &library.id,
            vec![
                DocumentInput {
                    filename: "ok.md".into(),
                    content: "# Ok\n\nFine.".into(),
                    title: None,
                },
                DocumentInput {
                    filename: "empty.md".into(),
                    content: String::new(),
                    title: None,
                },
            ],
        )
        .await;
    match result {
        Err(AppError::BadRequest(message)) => assert!(message.contains("empty.md"), "{message}"),
        other => panic!("expected BadRequest naming the empty document, got {other:?}"),
    }
}

#[tokio::test]
async fn a_batch_with_the_same_name_but_different_content_is_rejected() {
    let (_tempdir, service, _runtime) = service_fixture().await;
    let library = service
        .create_library(CreateLibraryRequest {
            name: "批内冲突库".into(),
            description: None,
        })
        .await
        .unwrap();
    let result = service
        .submit_documents(
            &library.id,
            vec![
                DocumentInput {
                    filename: "a.md".into(),
                    content: "one version".into(),
                    title: None,
                },
                DocumentInput {
                    filename: "a.md".into(),
                    content: "another version".into(),
                    title: None,
                },
            ],
        )
        .await;
    assert!(matches!(result, Err(AppError::Conflict(_))));
}

#[tokio::test]
async fn a_batch_with_identical_duplicate_entries_collapses_to_one() {
    let (_tempdir, service, runtime) = service_fixture().await;
    let library = service
        .create_library(CreateLibraryRequest {
            name: "批内折叠库".into(),
            description: None,
        })
        .await
        .unwrap();
    let batch = vec![
        DocumentInput {
            filename: "a.md".into(),
            content: "# A\n\nFirst source.".into(),
            title: None,
        },
        DocumentInput {
            filename: "a.md".into(),
            content: "# A\n\nFirst source.".into(),
            title: None,
        },
    ];
    let submitted = service.submit_documents(&library.id, batch).await.unwrap();
    assert_eq!(submitted.documents.len(), 1, "{:?}", submitted.documents);
    let status = wait_for_completion(&service, &library.id, &submitted.job_id).await;
    assert_eq!(status.status, JobState::Completed, "{status:?}");
    let ingest_sessions = runtime
        .requests
        .lock()
        .unwrap()
        .iter()
        .filter(|request| request.title.contains("ingestion"))
        .count();
    assert_eq!(ingest_sessions, 1);
}

#[tokio::test]
async fn a_batch_with_duplicate_content_across_names_reports_both_entries_and_one_ingest() {
    let (_tempdir, service, runtime) = service_fixture().await;
    let library = service
        .create_library(CreateLibraryRequest {
            name: "跨名重复批库".into(),
            description: None,
        })
        .await
        .unwrap();
    let submitted = service
        .submit_documents(
            &library.id,
            vec![
                DocumentInput {
                    filename: "a.md".into(),
                    content: "# Shared\n\nOne content.".into(),
                    title: None,
                },
                DocumentInput {
                    filename: "b.md".into(),
                    content: "# Shared\n\nOne content.".into(),
                    title: None,
                },
            ],
        )
        .await
        .unwrap();
    // sha256 dedupe maps the second entry onto the first's stored record:
    // both entries are reported, but the job covers the shared path once.
    assert_eq!(submitted.documents.len(), 2, "{:?}", submitted.documents);
    assert!(
        !submitted.documents[0].duplicate,
        "{:?}",
        submitted.documents
    );
    assert!(
        submitted.documents[1].duplicate,
        "{:?}",
        submitted.documents
    );
    assert_eq!(
        submitted.documents[0].document_path,
        submitted.documents[1].document_path
    );
    let status = wait_for_completion(&service, &library.id, &submitted.job_id).await;
    assert_eq!(status.status, JobState::Completed, "{status:?}");
    let prompts: Vec<String> = runtime
        .requests
        .lock()
        .unwrap()
        .iter()
        .filter(|request| request.title.contains("ingestion"))
        .map(|request| request.prompt.clone())
        .collect();
    assert_eq!(prompts.len(), 1, "{prompts:?}");
    assert!(prompts[0].contains("raw/a.md"), "{}", prompts[0]);
    assert!(!prompts[0].contains("raw/b.md"), "{}", prompts[0]);
}

#[tokio::test]
async fn a_failed_batch_is_recompiled_by_the_next_ingest_via_extras() {
    let (_tempdir, service, runtime) = service_fixture().await;
    let library = service
        .create_library(CreateLibraryRequest {
            name: "批量补编译库".into(),
            description: None,
        })
        .await
        .unwrap();
    runtime.fail_next_ingests.store(1, Ordering::SeqCst);

    let failed = service
        .submit_documents(
            &library.id,
            vec![
                DocumentInput {
                    filename: "doc1.md".into(),
                    content: "# One\n\nFirst source.".into(),
                    title: None,
                },
                DocumentInput {
                    filename: "doc2.md".into(),
                    content: "# Two\n\nSecond source.".into(),
                    title: None,
                },
            ],
        )
        .await
        .unwrap();
    let failed_status = wait_for_completion(&service, &library.id, &failed.job_id).await;
    assert_eq!(failed_status.status, JobState::Failed, "{failed_status:?}");

    // The failed batch left both documents in raw/ without any node; the
    // next submission's prompt names them alongside the new document.
    let next = service
        .submit_documents(
            &library.id,
            vec![DocumentInput {
                filename: "doc3.md".into(),
                content: "# Three\n\nThird source.".into(),
                title: None,
            }],
        )
        .await
        .unwrap();
    let next_status = wait_for_completion(&service, &library.id, &next.job_id).await;
    assert_eq!(next_status.status, JobState::Completed, "{next_status:?}");

    let prompts: Vec<String> = runtime
        .requests
        .lock()
        .unwrap()
        .iter()
        .filter(|request| request.title.contains("ingestion"))
        .map(|request| request.prompt.clone())
        .collect();
    assert_eq!(prompts.len(), 2, "{prompts:?}");
    for path in ["raw/doc1.md", "raw/doc2.md", "raw/doc3.md"] {
        assert!(prompts[1].contains(path), "{}", prompts[1]);
    }
}

#[tokio::test]
async fn document_submission_is_served_over_http() {
    let (_tempdir, service, _runtime) = service_fixture().await;
    service
        .create_library(CreateLibraryRequest {
            name: "batchlib".into(),
            description: None,
        })
        .await
        .unwrap();
    let app = http_api::router(service);

    let body = serde_json::json!({
        "documents": [
            { "filename": "a.md", "content": "# A\n\nFirst source." },
            { "filename": "b.md", "content": "# B\n\nSecond source." }
        ]
    });
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/libraries/batchlib/documents")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(value["library_id"], "batchlib");
    assert_eq!(value["documents"].as_array().unwrap().len(), 2);
    let job_id = value["job_id"].as_str().unwrap().to_string();

    // Poll the job over HTTP too, so the batch's ingest finishes before
    // the fixture is torn down.
    for _ in 0..500 {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/v1/libraries/batchlib/jobs/{job_id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let status: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        match status["status"].as_str() {
            Some("completed") => return,
            Some("queued") | Some("running") => {}
            _ => panic!("batch job did not complete: {status}"),
        }
        sleep(Duration::from_millis(10)).await;
    }
    panic!("batch ingestion job did not finish: {job_id}");
}

#[tokio::test]
async fn an_uncompiled_duplicate_is_reingested_instead_of_skipped() {
    let (_tempdir, service, runtime) = service_fixture().await;
    let library = service
        .create_library(CreateLibraryRequest {
            name: "重复补摄入库".into(),
            description: None,
        })
        .await
        .unwrap();
    runtime.fail_next_ingests.store(1, Ordering::SeqCst);
    let document = DocumentInput {
        filename: "source.md".into(),
        content: "# Session Context\n\nTest source.".into(),
        title: None,
    };

    let first = service
        .submit_documents(&library.id, vec![document.clone()])
        .await
        .unwrap();
    assert!(!first.documents[0].duplicate);
    let first_status = wait_for_completion(&service, &library.id, &first.job_id).await;
    assert_eq!(first_status.status, JobState::Failed, "{first_status:?}");

    // Same content again: dedupe sees a duplicate, but no wiki node
    // references it, so the submission runs a real ingestion instead of
    // skipping.
    let second = service
        .submit_documents(&library.id, vec![document.clone()])
        .await
        .unwrap();
    assert!(second.documents[0].duplicate && !second.documents[0].skipped);
    let second_status = wait_for_completion(&service, &library.id, &second.job_id).await;
    assert_eq!(
        second_status.status,
        JobState::Completed,
        "{second_status:?}"
    );

    // The node exists now: the third submission is the genuine no-op skip.
    let third = service
        .submit_documents(&library.id, vec![document])
        .await
        .unwrap();
    assert!(third.documents[0].duplicate && third.documents[0].skipped);
    assert_eq!(
        service
            .job_status(&library.id, &third.job_id)
            .unwrap()
            .status,
        JobState::Skipped
    );
}
