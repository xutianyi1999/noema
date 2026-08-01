//! Offline end-to-end tests: HTTP JSON API + Streamable HTTP MCP + service +
//! storage, driven by a fake OpenCode runtime. These never need a model,
//! network, or graphify binary. The live OpenCode flow lives in e2e_live.rs.

use std::{
    fs,
    net::SocketAddr,
    path::{Path, PathBuf},
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
    models::JobStatus,
    runtime::{AgentRunRequest, AgentRunResult, OpenCodeRuntime},
};
use rmcp::{
    Peer, RoleClient, model::CallToolRequestParams, serve_client,
    transport::StreamableHttpClientTransport,
};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::time::sleep;
use tower::ServiceExt;

// ---------------------------------------------------------------------------
// Fake OpenCode runtime
// ---------------------------------------------------------------------------

/// Behavior is selected by markers in the document content (ingest) or the
/// user prompt (query):
///
/// - `FAIL_RUNTIME`  — the session fails before producing anything.
/// - `FAIL_QUERY`    — a query session fails.
/// - `BAD_NODE`      — ingest writes a wiki node without frontmatter.
/// - `TOUCH_RAW`     — ingest modifies a protected `raw/` file in staging.
/// - `STRAY_FILE`    — ingest leaves an unauthorized top-level file.
/// - `STRAY_LINK`    — ingest leaves a symlink outside `.opencode`.
#[derive(Default)]
struct FakeRuntime {
    next_session: AtomicUsize,
    requests: Mutex<Vec<AgentRunRequest>>,
}

/// Like the real Agent, the fake only reads the document the prompt points
/// at — not every file in `raw/`. A failed document legitimately stays in
/// `raw/` and must not influence later jobs.
fn corpus_of(request: &AgentRunRequest) -> String {
    let mut corpus = request.prompt.clone();
    for token in request.prompt.split_whitespace() {
        let candidate = token.trim_matches(
            |c: char| !matches!(c, 'a'..='z' | 'A'..='Z' | '0'..='9' | '/' | '-' | '_' | '.'),
        );
        if candidate.starts_with("raw/")
            && let Ok(contents) = fs::read_to_string(request.workdir.join(candidate))
        {
            corpus.push_str(&contents);
        }
    }
    corpus
}

fn first_raw_name(workdir: &Path) -> Option<String> {
    let mut names: Vec<String> = fs::read_dir(workdir.join("raw"))
        .ok()?
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .collect();
    names.sort();
    names.into_iter().next()
}

fn write_valid_node(workdir: &Path, source: &str) {
    let stem = Path::new(source)
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("concept");
    let node = format!(
        "---\nnode_id: {stem}\ncanonical_name: {stem}\nkind: concept\nsources:\n  - path: raw/{source}\nrelations:\n  depends_on: []\n  related_to: []\n  opposite_to: []\nclaim_type: observed\nconfidence: 1.0\ncreated_at: 2026-08-01T00:00:00Z\nupdated_at: 2026-08-01T00:00:00Z\n---\n\n# {stem}\n\nA compiled knowledge node.\n\n## Evidence\n\n- raw/{source}\n\n## RAG Version\n\nCompiled from raw/{source}.\n"
    );
    fs::write(workdir.join(format!("wiki/{stem}.md")), node).unwrap();
}

impl OpenCodeRuntime for FakeRuntime {
    async fn run_new_session(&self, request: AgentRunRequest) -> Result<AgentRunResult, AppError> {
        let number = self.next_session.fetch_add(1, Ordering::SeqCst) + 1;
        let session_id = format!("fake-session-{number}");
        let is_ingest = request.title.contains("ingestion");
        let corpus = corpus_of(&request);
        self.requests.lock().unwrap().push(request.clone());

        if corpus.contains("FAIL_RUNTIME") {
            return Err(AppError::Runtime("simulated OpenCode failure".into()));
        }
        if !is_ingest && corpus.contains("FAIL_QUERY") {
            return Err(AppError::Runtime("simulated query failure".into()));
        }

        let source = first_raw_name(&request.workdir).unwrap_or_default();
        if is_ingest {
            if corpus.contains("BAD_NODE") {
                fs::write(
                    request.workdir.join("wiki/bad-node.md"),
                    "# no frontmatter here\n",
                )
                .unwrap();
            } else {
                write_valid_node(&request.workdir, &source);
            }
            fs::write(
                request.workdir.join("graphify-out/graph.json"),
                r#"{"nodes":[{"id":"session-context"}],"links":[]}"#,
            )
            .unwrap();
            if corpus.contains("TOUCH_RAW") {
                let target = request.workdir.join("raw").join(&source);
                let mut contents = fs::read_to_string(&target).unwrap();
                contents.push_str("\nagent tampering\n");
                fs::write(target, contents).unwrap();
            }
            if corpus.contains("STRAY_FILE") {
                fs::write(request.workdir.join("stray.txt"), "unauthorized\n").unwrap();
            }
            if corpus.contains("STRAY_LINK") {
                std::os::unix::fs::symlink("/etc", request.workdir.join("evil-link")).unwrap();
            }
        }

        let answer = if is_ingest {
            "摄入完成".to_string()
        } else if source.is_empty() {
            "当前内容库还没有来源。".to_string()
        } else {
            let stem = Path::new(&source)
                .file_stem()
                .and_then(|value| value.to_str())
                .unwrap_or("concept");
            // Cite like a real model: locators, backticks, Chinese
            // punctuation, several references on one line.
            format!(
                "依据内容库，结论如下：证据见 `raw/{source}:3-5,11`；节点见 `wiki/{stem}.md:12`。引用：raw/{source}:3；wiki/{stem}.md。"
            )
        };

        Ok(AgentRunResult {
            session_id,
            answer,
            tool_events: Vec::new(),
        })
    }
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn config(data_dir: PathBuf) -> Config {
    Config {
        data_dir,
        bind: "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
        opencode_url: "http://127.0.0.1:4096".into(),
        opencode_model: "opencode/deepseek-v4-flash-free".into(),
        opencode_timeout_secs: 5,
        graphify_bin: "graphify".into(),
        install_graphify: false,
    }
}

struct Fixture {
    _tempdir: TempDir,
    service: AppService<FakeRuntime>,
    runtime: Arc<FakeRuntime>,
    api: Api,
}

async fn fixture() -> Fixture {
    let tempdir = tempfile::tempdir().unwrap();
    let runtime = Arc::new(FakeRuntime::default());
    let service =
        AppService::with_runtime(config(tempdir.path().join("data")), runtime.clone()).unwrap();
    let api = Api {
        app: http_api::router(service.clone()),
    };
    Fixture {
        _tempdir: tempdir,
        service,
        runtime,
        api,
    }
}

async fn create_library(api: &Api, name: &str) -> Value {
    let (status, body) = api
        .call("POST", "/v1/libraries", Some(json!({ "name": name })))
        .await;
    assert_eq!(status, StatusCode::OK, "create library failed: {body}");
    body
}

async fn submit(api: &Api, library_id: &str, filename: &str, content: &str) -> Value {
    let (status, body) = api
        .call(
            "POST",
            &format!("/v1/libraries/{library_id}/documents"),
            Some(json!({ "filename": filename, "content": content })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "submit failed: {body}");
    body
}

async fn wait_job(api: &Api, library_id: &str, job_id: &str) -> JobStatus {
    for _ in 0..200 {
        let (status, body) = api
            .call(
                "GET",
                &format!("/v1/libraries/{library_id}/jobs/{job_id}"),
                None,
            )
            .await;
        assert_eq!(status, StatusCode::OK, "job poll failed: {body}");
        let job: JobStatus = serde_json::from_value(body).unwrap();
        if matches!(job.status.as_str(), "completed" | "failed" | "skipped") {
            return job;
        }
        sleep(Duration::from_millis(10)).await;
    }
    panic!("job {job_id} did not finish");
}

// ---------------------------------------------------------------------------
// HTTP helpers
// ---------------------------------------------------------------------------

struct Api {
    app: axum::Router,
}

impl Api {
    async fn call(&self, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
        let mut builder = Request::builder().method(method).uri(uri);
        let payload = match body {
            Some(value) => {
                builder = builder.header(header::CONTENT_TYPE, "application/json");
                Body::from(value.to_string())
            }
            None => Body::empty(),
        };
        let response = self
            .app
            .clone()
            .oneshot(builder.body(payload).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes)
                .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()))
        };
        (status, value)
    }
}

// ---------------------------------------------------------------------------
// Streamable HTTP MCP helpers (official rmcp client against a real socket)
// ---------------------------------------------------------------------------

async fn spawn_server(service: AppService<FakeRuntime>) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, http_api::router(service))
            .await
            .unwrap();
    });
    address
}

async fn mcp_client(address: SocketAddr) -> rmcp::service::RunningService<RoleClient, ()> {
    let transport = StreamableHttpClientTransport::from_uri(format!("http://{address}/mcp"));
    serve_client((), transport)
        .await
        .expect("MCP initialize handshake must succeed")
}

/// Calls a tool and returns `(is_error, parsed first text content)`. Both
/// MCP tool errors (`isError: true` results) and JSON-RPC level errors are
/// reported through the `is_error` flag.
async fn call_tool(client: &Peer<RoleClient>, name: &str, arguments: Value) -> (bool, Value) {
    let mut params = CallToolRequestParams::new(name.to_string());
    if let Value::Object(map) = arguments {
        params = params.with_arguments(map);
    }
    let result = match client.call_tool(params).await {
        Ok(result) => result,
        Err(error) => return (true, Value::String(error.to_string())),
    };
    let is_error = result.is_error.unwrap_or(false);
    let text = result
        .content
        .iter()
        .find_map(|content| content.as_text())
        .map(|text| text.text.clone())
        .unwrap_or_default();
    let parsed = serde_json::from_str(&text).unwrap_or(Value::String(text));
    (is_error, parsed)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn e2e_http_health_and_library_scaffold() {
    let fx = fixture().await;

    let (status, body) = fx.api.call("GET", "/v1/health", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "ok");
    assert_eq!(body["configured_model"], "opencode/deepseek-v4-flash-free");

    let library = create_library(&fx.api, "产品知识库").await;
    let root = PathBuf::from(library["root"].as_str().unwrap());
    for relative in [
        "purpose.md",
        "schema.md",
        "index.md",
        ".graphifyignore",
        "manifest.json",
        "library.sqlite",
        ".opencode/skills/kb-ingest/SKILL.md",
        ".opencode/skills/kb-query/SKILL.md",
        ".opencode/skills/kb-maintain/SKILL.md",
        ".opencode/skills/knowledge-compiler/SKILL.md",
        "raw",
        "wiki",
        "graph",
        "index",
        "reviews",
        "staging",
        "graphify-out",
    ] {
        assert!(
            root.join(relative).exists(),
            "missing scaffold entry {relative}"
        );
    }
    let ignore = fs::read_to_string(root.join(".graphifyignore")).unwrap();
    assert!(ignore.contains("!raw/") && ignore.contains("!wiki/"));

    let (status, body) = fx
        .api
        .call("POST", "/v1/libraries", Some(json!({ "name": "   " })))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
}

#[tokio::test]
async fn e2e_http_ingest_poll_query_flow() {
    let fx = fixture().await;
    let library = create_library(&fx.api, "端到端").await;
    let library_id = library["id"].as_str().unwrap();
    let root = PathBuf::from(library["root"].as_str().unwrap());

    let submitted = submit(
        &fx.api,
        library_id,
        "source.md",
        "# Session Context\n\nTest source about session context.",
    )
    .await;
    assert_eq!(submitted["duplicate"], false);
    assert_eq!(submitted["library_id"], library_id);
    let document_path = submitted["document_path"].as_str().unwrap().to_string();

    let job = wait_job(&fx.api, library_id, submitted["job_id"].as_str().unwrap()).await;
    assert_eq!(job.status, "completed", "{job:?}");
    assert!(job.session_id.is_some());
    assert_eq!(job.kind, "ingest");
    assert!(job.error.is_none());

    // Raw text is immutable and addressable.
    let raw_contents = fs::read_to_string(root.join(&document_path)).unwrap();
    assert!(raw_contents.contains("Test source about session context."));

    // Knowledge artifacts were promoted from staging.
    let wiki_files: Vec<_> = fs::read_dir(root.join("wiki"))
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .collect();
    assert_eq!(wiki_files.len(), 1, "{wiki_files:?}");
    let node = fs::read_to_string(root.join("wiki").join(&wiki_files[0])).unwrap();
    for key in [
        "node_id:",
        "canonical_name:",
        "kind:",
        "sources:",
        "relations:",
        "claim_type:",
        "confidence:",
        "created_at:",
        "updated_at:",
    ] {
        assert!(node.contains(key), "node misses {key}");
    }
    assert!(root.join("graphify-out/graph.json").is_file());

    // Navigation index and manifest reflect the ingestion.
    let index = fs::read_to_string(root.join("index.md")).unwrap();
    assert!(index.contains(&document_path), "{index}");
    assert!(
        index.contains(&format!("wiki/{}", wiki_files[0])),
        "{index}"
    );
    let manifest: Value =
        serde_json::from_str(&fs::read_to_string(root.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(manifest["documents"].as_array().unwrap().len(), 1);

    // Full-text index was rebuilt.
    let connection = rusqlite::Connection::open(root.join("library.sqlite")).unwrap();
    let fts_count: i64 = connection
        .query_row("SELECT count(*) FROM content_fts", [], |row| row.get(0))
        .unwrap();
    assert!(fts_count >= 2, "expected raw + wiki rows, got {fts_count}");

    // Staging is cleaned up after success.
    let staging_leftovers: Vec<_> = fs::read_dir(root.join("staging"))
        .unwrap()
        .flatten()
        .collect();
    assert!(staging_leftovers.is_empty(), "{staging_leftovers:?}");

    // Two queries see the same knowledge but never share a session.
    let (status, first) = fx
        .api
        .call(
            "POST",
            &format!("/v1/libraries/{library_id}/query"),
            Some(json!({ "prompt": "这个概念是什么？" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{first}");
    let (status, second) = fx
        .api
        .call(
            "POST",
            &format!("/v1/libraries/{library_id}/query"),
            Some(json!({ "prompt": "它的证据来源有哪些？" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{second}");

    assert_ne!(first["session_id"], second["session_id"]);
    assert_eq!(first["library_id"], library_id);
    let references = first["references"].as_array().unwrap();
    assert!(references.len() >= 2, "{first}");
    let raw_reference = references
        .iter()
        .find(|reference| reference["source"] == json!(document_path))
        .unwrap_or_else(|| panic!("missing raw reference in {first}"));
    assert_eq!(
        raw_reference["node"],
        json!(format!("wiki/{}", wiki_files[0]))
    );
    // Locator suffixes and punctuation must not leak into sources.
    for reference in references {
        let source = reference["source"].as_str().unwrap();
        assert!(
            source.ends_with(".md") || source.ends_with(".txt"),
            "unparsed locator in reference: {reference}"
        );
        assert!(
            !source.contains(':'),
            "locator leaked into source: {source}"
        );
    }
    assert!(
        references
            .iter()
            .any(|reference| reference["source"] == json!(format!("wiki/{}", wiki_files[0])))
    );

    // Neither query prompt may carry the other query's conversation history.
    let requests = fx.runtime.requests.lock().unwrap();
    let query_prompts: Vec<&str> = requests
        .iter()
        .filter(|request| request.title.contains("query"))
        .map(|request| request.prompt.as_str())
        .collect();
    assert_eq!(query_prompts.len(), 2);
    assert!(query_prompts[0].contains("这个概念是什么？"));
    assert!(!query_prompts[1].contains("这个概念是什么？"));
    assert!(query_prompts[0].contains("purpose.md"));
    for request in requests.iter() {
        assert_eq!(request.library_id, library_id);
        if request.title.contains("query") {
            // Queries run directly at the library root.
            assert_eq!(request.workdir, root);
        } else {
            // Ingestion runs inside a job-scoped staging project.
            assert!(request.workdir.starts_with(root.join("staging")));
        }
    }
}

#[tokio::test]
async fn e2e_http_duplicate_and_invalid_documents() {
    let fx = fixture().await;
    let library = create_library(&fx.api, "去重").await;
    let library_id = library["id"].as_str().unwrap();
    let root = PathBuf::from(library["root"].as_str().unwrap());
    let content = "# 同一份内容\n\n完全相同的正文。";

    let first = submit(&fx.api, library_id, "one.md", content).await;
    assert_eq!(first["duplicate"], false);
    let job = wait_job(&fx.api, library_id, first["job_id"].as_str().unwrap()).await;
    assert_eq!(job.status, "completed", "{job:?}");

    let second = submit(&fx.api, library_id, "renamed.txt", content).await;
    assert_eq!(second["duplicate"], true);
    assert_eq!(second["document_path"], first["document_path"]);
    let job = wait_job(&fx.api, library_id, second["job_id"].as_str().unwrap()).await;
    assert_eq!(job.status, "skipped", "{job:?}");
    assert!(job.session_id.is_none());

    let raw_files: Vec<_> = fs::read_dir(root.join("raw")).unwrap().flatten().collect();
    assert_eq!(
        raw_files.len(),
        1,
        "duplicate must not create a new raw file"
    );

    // The runtime was invoked exactly once (for the first submission).
    assert_eq!(fx.runtime.requests.lock().unwrap().len(), 1);

    for (filename, reason) in [
        ("../escape.md", "path traversal"),
        ("nested/file.md", "subdirectory"),
        ("back\\slash.md", "backslash"),
        ("binary.pdf", "wrong extension"),
        (".", "dot"),
        ("", "empty"),
    ] {
        let (status, body) = fx
            .api
            .call(
                "POST",
                &format!("/v1/libraries/{library_id}/documents"),
                Some(json!({ "filename": filename, "content": "x" })),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{reason}: {body}");
    }

    let (status, body) = fx
        .api
        .call(
            "POST",
            &format!("/v1/libraries/{library_id}/documents"),
            Some(json!({ "filename": "empty.md", "content": "" })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "empty content: {body}");
}

#[tokio::test]
async fn e2e_http_error_paths() {
    let fx = fixture().await;
    let library = create_library(&fx.api, "错误路径").await;
    let library_id = library["id"].as_str().unwrap();

    let (status, _) = fx
        .api
        .call(
            "POST",
            &format!("/v1/libraries/{library_id}/query"),
            Some(json!({ "prompt": "   " })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, body) = fx
        .api
        .call(
            "POST",
            "/v1/libraries/no-such-library/query",
            Some(json!({ "prompt": "你好" })),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");

    let (status, _) = fx
        .api
        .call(
            "POST",
            "/v1/libraries/UPPERCASE/query",
            Some(json!({ "prompt": "你好" })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, _) = fx
        .api
        .call(
            "GET",
            "/v1/libraries/no-such-library/jobs/00000000-0000-0000-0000-000000000000",
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, _) = fx
        .api
        .call(
            "GET",
            &format!("/v1/libraries/{library_id}/jobs/does-not-exist"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Malformed JSON body.
    let response = fx
        .api
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/libraries")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from("{not json"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn e2e_http_library_isolation_and_delete() {
    let fx = fixture().await;
    let library_a = create_library(&fx.api, "隔离 A").await;
    let library_b = create_library(&fx.api, "隔离 B").await;
    let id_a = library_a["id"].as_str().unwrap().to_string();
    let id_b = library_b["id"].as_str().unwrap().to_string();
    let root_a = PathBuf::from(library_a["root"].as_str().unwrap());
    let root_b = PathBuf::from(library_b["root"].as_str().unwrap());

    let submitted = submit(
        &fx.api,
        &id_a,
        "alpha.md",
        "# Alpha\n\nLibrary A secret: 天问一号。",
    )
    .await;
    let job = wait_job(&fx.api, &id_a, submitted["job_id"].as_str().unwrap()).await;
    assert_eq!(job.status, "completed", "{job:?}");
    let submitted_b = submit(
        &fx.api,
        &id_b,
        "beta.md",
        "# Beta\n\nLibrary B secret: 祝融号。",
    )
    .await;
    let job_b = wait_job(&fx.api, &id_b, submitted_b["job_id"].as_str().unwrap()).await;
    assert_eq!(job_b.status, "completed", "{job_b:?}");

    // Jobs never cross the library boundary.
    let (status, _) = fx
        .api
        .call(
            "GET",
            &format!("/v1/libraries/{id_b}/jobs/{}", job.job_id),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Query B: the agent only ever sees B's workspace and B's knowledge.
    let (status, answer_b) = fx
        .api
        .call(
            "POST",
            &format!("/v1/libraries/{id_b}/query"),
            Some(json!({ "prompt": "B 库里有什么？" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{answer_b}");
    assert!(!answer_b["answer"].as_str().unwrap().contains("alpha"));

    {
        let requests = fx.runtime.requests.lock().unwrap();
        for request in requests.iter() {
            if request.library_id == id_a {
                assert!(request.workdir.starts_with(&root_a));
                assert!(!request.workdir.starts_with(&root_b));
            } else {
                assert_eq!(request.library_id, id_b);
                assert!(request.workdir.starts_with(&root_b));
            }
            if request.title.contains("query") {
                assert!(
                    !request.prompt.contains("天问一号"),
                    "query prompt must not leak other libraries' content"
                );
            }
        }
    }

    // B's raw directory never received A's document.
    for entry in fs::read_dir(root_b.join("raw")).unwrap().flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        assert!(name.contains("beta.md"), "unexpected raw file in B: {name}");
    }

    // Deleting A leaves B untouched.
    fx.service.storage.discard_library(&id_a).unwrap();
    assert!(!root_a.exists());
    assert!(root_b.join("raw").read_dir().unwrap().next().is_some());
    let (status, _) = fx
        .api
        .call(
            "POST",
            &format!("/v1/libraries/{id_a}/query"),
            Some(json!({ "prompt": "还在吗？" })),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, body) = fx
        .api
        .call(
            "POST",
            &format!("/v1/libraries/{id_b}/query"),
            Some(json!({ "prompt": "B 还活着吗？" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

#[tokio::test]
async fn e2e_ingest_runtime_failure_preserves_state() {
    let fx = fixture().await;
    let library = create_library(&fx.api, "失败重试").await;
    let library_id = library["id"].as_str().unwrap().to_string();
    let root = PathBuf::from(library["root"].as_str().unwrap());
    let content = "# 会失败的文档\n\nFAIL_RUNTIME marker inside.";

    let submitted = submit(&fx.api, &library_id, "boom.md", content).await;
    let job = wait_job(&fx.api, &library_id, submitted["job_id"].as_str().unwrap()).await;
    assert_eq!(job.status, "failed", "{job:?}");
    assert!(job.error.unwrap().contains("simulated OpenCode failure"));
    assert!(job.session_id.is_none());

    // Failed ingestion must not promote anything and must keep the staging
    // directory for auditing.
    assert!(fs::read_dir(root.join("wiki")).unwrap().next().is_none());
    let staging_jobs: Vec<_> = fs::read_dir(root.join("staging"))
        .unwrap()
        .flatten()
        .collect();
    assert_eq!(staging_jobs.len(), 1, "failed job keeps its staging dir");

    // The raw document survives untouched.
    let raw_files: Vec<_> = fs::read_dir(root.join("raw")).unwrap().flatten().collect();
    assert_eq!(raw_files.len(), 1);

    // Re-submitting the same content is a hash-duplicate skip: the original
    // document is not overwritten and no new session runs.
    let resubmit = submit(&fx.api, &library_id, "boom-retry.md", content).await;
    assert_eq!(resubmit["duplicate"], true);
    let job = wait_job(&fx.api, &library_id, resubmit["job_id"].as_str().unwrap()).await;
    assert_eq!(job.status, "skipped");

    // A different document still ingests fine, with a brand-new session.
    let ok = submit(&fx.api, &library_id, "good.md", "# 正常文档\n\n可以编译。").await;
    let job = wait_job(&fx.api, &library_id, ok["job_id"].as_str().unwrap()).await;
    assert_eq!(job.status, "completed", "{job:?}");
    let completed_session = job.session_id.unwrap();

    let session_ids: Vec<String> = fx
        .runtime
        .requests
        .lock()
        .unwrap()
        .iter()
        .enumerate()
        .map(|(index, _)| format!("fake-session-{}", index + 1))
        .collect();
    let unique: std::collections::HashSet<_> = session_ids.iter().collect();
    assert_eq!(
        session_ids.len(),
        unique.len(),
        "session ids must be unique"
    );
    assert!(session_ids.contains(&completed_session));
}

#[tokio::test]
async fn e2e_ingest_validation_rejects_bad_agent_output() {
    for (marker, reason) in [
        ("BAD_NODE", "wiki node without frontmatter"),
        ("TOUCH_RAW", "agent modified protected raw/"),
        ("STRAY_FILE", "unauthorized top-level file"),
        ("STRAY_LINK", "symlink escape attempt"),
    ] {
        let fx = fixture().await;
        let library = create_library(&fx.api, "校验").await;
        let library_id = library["id"].as_str().unwrap().to_string();
        let root = PathBuf::from(library["root"].as_str().unwrap());

        let submitted = submit(
            &fx.api,
            &library_id,
            "attack.md",
            &format!("# 攻击样本\n\n{marker} marker."),
        )
        .await;
        let job = wait_job(&fx.api, &library_id, submitted["job_id"].as_str().unwrap()).await;
        assert_eq!(job.status, "failed", "{reason}: {job:?}");
        assert!(job.error.is_some(), "{reason}");
        // The failed session id is still recorded for auditability.
        assert!(job.session_id.is_some(), "{reason}");

        // Nothing was promoted to the canonical knowledge directories.
        assert!(
            fs::read_dir(root.join("wiki")).unwrap().next().is_none(),
            "{reason}: wiki must stay empty"
        );
        assert!(
            fs::read_dir(root.join("graphify-out"))
                .unwrap()
                .next()
                .is_none(),
            "{reason}: graphify-out must stay empty"
        );
        // Staging is preserved for audit.
        assert!(
            fs::read_dir(root.join("staging")).unwrap().next().is_some(),
            "{reason}: staging must be preserved"
        );

        // The service still accepts a clean document afterwards.
        let ok = submit(&fx.api, &library_id, "clean.md", "# 干净文档\n\n正常内容。").await;
        let job = wait_job(&fx.api, &library_id, ok["job_id"].as_str().unwrap()).await;
        assert_eq!(job.status, "completed", "{reason}: recovery: {job:?}");
    }
}

#[tokio::test]
async fn e2e_query_failure_is_audited() {
    let fx = fixture().await;
    let library = create_library(&fx.api, "审计").await;
    let library_id = library["id"].as_str().unwrap().to_string();
    let root = PathBuf::from(library["root"].as_str().unwrap());

    let submitted = submit(&fx.api, &library_id, "doc.md", "# 文档\n\n可查询内容。").await;
    let job = wait_job(&fx.api, &library_id, submitted["job_id"].as_str().unwrap()).await;
    assert_eq!(job.status, "completed");

    let (status, body) = fx
        .api
        .call(
            "POST",
            &format!("/v1/libraries/{library_id}/query"),
            Some(json!({ "prompt": "FAIL_QUERY now" })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
    assert!(body["error"].as_str().unwrap().contains("query failed"));

    let (status, ok_body) = fx
        .api
        .call(
            "POST",
            &format!("/v1/libraries/{library_id}/query"),
            Some(json!({ "prompt": "正常问题" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{ok_body}");

    let control = fx._tempdir.path().join("data/control.sqlite");
    let connection = rusqlite::Connection::open(control).unwrap();
    let mut statement = connection
        .prepare("SELECT status, session_id, error FROM query_runs ORDER BY created_at")
        .unwrap();
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!(rows[0].0, "failed");
    assert!(
        rows[0]
            .2
            .as_deref()
            .unwrap()
            .contains("simulated query failure")
    );
    assert_eq!(rows[1].0, "completed");
    assert!(rows[1].1.is_some());
    assert!(root.join("wiki").read_dir().unwrap().next().is_some());
}

#[tokio::test]
async fn e2e_mcp_streamable_http_full_flow() {
    let fx = fixture().await;
    let address = spawn_server(fx.service.clone()).await;
    let client = mcp_client(address).await;

    // The server identifies itself as Noema and announces tool support.
    let server_info = client.peer_info().expect("server info after initialize");
    let implementation = server_info
        .server_info
        .as_ref()
        .expect("serverInfo must be present");
    assert_eq!(implementation.name, "noema");
    assert_eq!(implementation.version, env!("CARGO_PKG_VERSION"));

    let tools = client.list_tools(None).await.unwrap();
    let names: Vec<String> = tools
        .tools
        .iter()
        .map(|tool| tool.name.to_string())
        .collect();
    for expected in [
        "kb_ingest_document",
        "kb_query",
        "kb_job_status",
        "kb_health",
    ] {
        assert!(
            names.iter().any(|name| name == expected),
            "missing MCP tool {expected}: {names:?}"
        );
    }

    let (is_error, health) = call_tool(&client, "kb_health", json!({})).await;
    assert!(!is_error);
    assert_eq!(health["status"], "ok");

    // Create the library over HTTP (MCP has no library-creation tool).
    let library = create_library(&fx.api, "MCP 库").await;
    let library_id = library["id"].as_str().unwrap().to_string();

    let (is_error, ingested) = call_tool(
        &client,
        "kb_ingest_document",
        json!({
            "library_id": library_id,
            "filename": "mcp.md",
            "content": "# MCP 文档\n\n通过 MCP 摄入。",
            "title": "MCP 文档"
        }),
    )
    .await;
    assert!(!is_error);
    assert_eq!(ingested["duplicate"], false);
    let job_id = ingested["job_id"].as_str().unwrap().to_string();

    let mut final_status = String::new();
    for _ in 0..200 {
        let (_, status) = call_tool(
            &client,
            "kb_job_status",
            json!({ "library_id": library_id, "job_id": job_id }),
        )
        .await;
        final_status = status["status"].as_str().unwrap().to_string();
        if matches!(final_status.as_str(), "completed" | "failed" | "skipped") {
            break;
        }
        sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(final_status, "completed");

    let (is_error, query) = call_tool(
        &client,
        "kb_query",
        json!({ "library_id": library_id, "prompt": "MCP 文档讲了什么？" }),
    )
    .await;
    assert!(!is_error);
    assert!(!query["answer"].as_str().unwrap().is_empty());
    assert!(
        !query["references"].as_array().unwrap().is_empty(),
        "{query}"
    );
    assert_eq!(query["library_id"], library_id);

    // Every kb_query call creates a new session.
    let (_, second) = call_tool(
        &client,
        "kb_query",
        json!({ "library_id": library_id, "prompt": "来源是哪个文件？" }),
    )
    .await;
    assert_ne!(query["session_id"], second["session_id"]);

    // Errors surface through the MCP tool channel as tool errors.
    let (is_error, payload) = call_tool(
        &client,
        "kb_query",
        json!({ "library_id": "no-such-library", "prompt": "你好" }),
    )
    .await;
    assert!(is_error, "{payload}");
    assert!(
        payload.to_string().contains("library not found"),
        "{payload}"
    );

    let (is_error, payload) = call_tool(
        &client,
        "kb_ingest_document",
        json!({
            "library_id": library_id,
            "filename": "../escape.md",
            "content": "x"
        }),
    )
    .await;
    assert!(is_error, "{payload}");
    assert!(
        payload.to_string().contains("single .md or .txt filename"),
        "{payload}"
    );
}
