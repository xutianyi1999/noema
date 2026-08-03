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
    /// Remaining ingest sessions to fail with a runtime error. Only ingest
    /// sessions consume the counter; queries pass through untouched.
    fail_next_ingests: AtomicUsize,
    /// Fired at the start of every session, ingest and query alike, with
    /// the request about to run; callbacks filter on the title themselves.
    on_session: Mutex<Option<SessionHook>>,
}

impl OpenCodeRuntime for FakeRuntime {
    async fn run_new_session(&self, request: AgentRunRequest) -> Result<AgentRunResult, AppError> {
        if let Some(hook) = self.on_session.lock().unwrap().as_ref() {
            hook(&request);
        }
        let number = self.next_session.fetch_add(1, Ordering::SeqCst) + 1;
        let session_id = format!("fake-session-{number}");
        let is_ingest = request.title.contains("ingestion");
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
        .submit_document(
            &library.id,
            DocumentInput {
                filename: "source.md".into(),
                content: "# Session Context\n\nTest source.".into(),
                title: title.map(str::to_string),
            },
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
    // Documents keep their original names: no hash prefix, no rewrite.
    assert!(root.join("raw/source.md").is_file());
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

    // Filenames are stable identities: the same name with different content
    // is rejected instead of coexisting under a hash prefix.
    service
        .submit_document(
            &first.id,
            DocumentInput {
                filename: "note.md".into(),
                content: "first version".into(),
                title: None,
            },
        )
        .await
        .unwrap();
    let conflict = service
        .submit_document(
            &first.id,
            DocumentInput {
                filename: "note.md".into(),
                content: "second, different version".into(),
                title: None,
            },
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
        .submit_document(
            &library.id,
            DocumentInput {
                filename: "one.md".into(),
                content: "# One\n\nFirst source.".into(),
                title: None,
            },
        )
        .await
        .unwrap();
    let second = service
        .submit_document(
            &library.id,
            DocumentInput {
                filename: "two.md".into(),
                content: "# Two\n\nSecond source.".into(),
                title: None,
            },
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
    let answer = service.query(&library.id, "bad-quote 场景").await.unwrap();
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
        .query(&library.id, "bad-contract 场景")
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
        .submit_document(
            &library.id,
            DocumentInput {
                filename: "source.md".into(),
                content: "# Session Context\n\nTest source.".into(),
                title: None,
            },
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
        .submit_document(
            &library.id,
            DocumentInput {
                filename: "doc1.md".into(),
                content: "# One\n\nFirst source.".into(),
                title: None,
            },
        )
        .await
        .unwrap();
    let first_status = wait_for_completion(&service, &library.id, &first.job_id).await;
    assert_eq!(first_status.status, JobState::Failed, "{first_status:?}");

    let second = service
        .submit_document(
            &library.id,
            DocumentInput {
                filename: "doc2.md".into(),
                content: "# Two\n\nSecond source.".into(),
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
        .submit_document(
            &library.id,
            DocumentInput {
                filename: "a.md".into(),
                content: "# A\n\nFirst source.".into(),
                title: None,
            },
        )
        .await
        .unwrap();
    let first_status = wait_for_completion(&service, &library.id, &first.job_id).await;
    assert_eq!(first_status.status, JobState::Failed, "{first_status:?}");

    // job2 compiles both a.md (named as an uncompiled extra) and b.md — the
    // fake runtime references every raw/ document from its node.
    let second = service
        .submit_document(
            &library.id,
            DocumentInput {
                filename: "b.md".into(),
                content: "# B\n\nSecond source.".into(),
                title: None,
            },
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
        .submit_document(
            &library.id,
            DocumentInput {
                filename: "b.md".into(),
                content: "# B\n\nSecond source.".into(),
                title: None,
            },
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
        .submit_document(&library.id, document.clone())
        .await
        .unwrap();
    assert!(!first.duplicate);
    let first_status = wait_for_completion(&service, &library.id, &first.job_id).await;
    assert_eq!(first_status.status, JobState::Failed, "{first_status:?}");

    // Same content again: dedupe sees a duplicate, but no wiki node
    // references it, so the submission runs a real ingestion instead of
    // skipping.
    let second = service
        .submit_document(&library.id, document.clone())
        .await
        .unwrap();
    assert!(!second.duplicate);
    let second_status = wait_for_completion(&service, &library.id, &second.job_id).await;
    assert_eq!(
        second_status.status,
        JobState::Completed,
        "{second_status:?}"
    );

    // The node exists now: the third submission is the genuine no-op skip.
    let third = service
        .submit_document(&library.id, document)
        .await
        .unwrap();
    assert!(third.duplicate);
    assert_eq!(
        service
            .job_status(&library.id, &third.job_id)
            .unwrap()
            .status,
        JobState::Skipped
    );
}
