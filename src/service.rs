use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use tokio::sync::Semaphore;

use crate::{
    answer,
    config::Config,
    error::AppError,
    models::{
        CreateLibraryRequest, DocumentInput, HealthResponse, JobKind, JobState, JobStatus, Library,
        QueryResponse, SubmitDocumentResponse,
    },
    references::safe_knowledge_path,
    runtime::{AgentRunRequest, OpenCodeAgent, OpenCodeRuntime},
    storage::{Storage, StoredDocument, referenced_sources},
};

pub struct AppService<R: OpenCodeRuntime> {
    pub storage: Storage,
    config: Arc<Config>,
    runtime: Arc<R>,
    /// One running ingest per library: promotion replaces `wiki/` and the
    /// graph wholesale, so same-library ingests must not overlap. Queued
    /// jobs wait on their library's lock and stay `queued` meanwhile.
    ingest_locks: Arc<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
    /// Global cap on concurrently running OpenCode sessions (ingests and
    /// queries alike), keeping bursts from swamping the managed server and
    /// the model backend behind it.
    sessions: Arc<Semaphore>,
}

impl<R: OpenCodeRuntime> Clone for AppService<R> {
    fn clone(&self) -> Self {
        Self {
            storage: self.storage.clone(),
            config: self.config.clone(),
            runtime: self.runtime.clone(),
            ingest_locks: self.ingest_locks.clone(),
            sessions: self.sessions.clone(),
        }
    }
}

/// How long after an ingest completes before staging is reconciled again:
/// OpenCode takes minutes to write session tombstones back into a deleted
/// project directory (observed ~4 min). Whatever this late pass misses, the
/// startup sweep covers.
const STAGING_RECONCILE_DELAY: Duration = Duration::from_secs(600);

impl AppService<OpenCodeAgent> {
    pub fn new(config: Config) -> Result<Self, AppError> {
        if config.max_sessions == 0 {
            return Err(AppError::BadRequest(
                "max_sessions must be at least 1".into(),
            ));
        }
        let config = Arc::new(config);
        let storage = Storage::open(config.data_dir.clone())?;
        let runtime = Arc::new(OpenCodeAgent::new(config.clone()));
        let sessions = Arc::new(Semaphore::new(config.max_sessions));
        let service = Self {
            storage,
            config,
            runtime,
            ingest_locks: Arc::new(Mutex::new(HashMap::new())),
            sessions,
        };
        service.startup_maintenance();
        Ok(service)
    }
}

impl<R: OpenCodeRuntime> AppService<R> {
    pub fn with_runtime(config: Config, runtime: Arc<R>) -> Result<Self, AppError> {
        if config.max_sessions == 0 {
            return Err(AppError::BadRequest(
                "max_sessions must be at least 1".into(),
            ));
        }
        let config = Arc::new(config);
        let storage = Storage::open(config.data_dir.clone())?;
        let sessions = Arc::new(Semaphore::new(config.max_sessions));
        let service = Self {
            storage,
            config,
            runtime,
            ingest_locks: Arc::new(Mutex::new(HashMap::new())),
            sessions,
        };
        service.startup_maintenance();
        Ok(service)
    }

    /// Liveness plus, when `detailed`, server internals. The HTTP probe
    /// passes `detailed` only for authorized callers (or when the API runs
    /// open); MCP tools always pass it — that endpoint is authenticated
    /// whenever the API is.
    pub fn health(&self, detailed: bool) -> HealthResponse {
        if detailed {
            HealthResponse {
                status: "ok",
                data_dir: Some(self.storage.root().display().to_string()),
                opencode_url: Some(self.config.opencode_url.clone()),
                configured_model: Some(self.config.opencode_model.clone()),
            }
        } else {
            HealthResponse {
                status: "ok",
                data_dir: None,
                opencode_url: None,
                configured_model: None,
            }
        }
    }

    /// Data directory this service manages (control plane + library roots),
    /// canonicalized by [`Storage::open`].
    pub fn data_dir(&self) -> &Path {
        self.storage.root()
    }

    /// The configured bearer token, when API authentication is enabled.
    pub fn auth_token(&self) -> Option<&str> {
        self.config.auth_token.as_deref()
    }

    /// The serialization lock for one content library, created on first
    /// use and shared by every ingest task targeting that library.
    fn ingest_lock(&self, library_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.ingest_locks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entry(library_id.to_string())
            .or_default()
            .clone()
    }

    /// Startup sweep, per library: drop leftover staging workspaces whose
    /// jobs no longer need them (see [`Storage::reconcile_staging`]), then
    /// refresh the Noema skills and the generated `AGENTS.md` contract to
    /// this binary's versions — which is also what converges libraries
    /// created by older binaries onto the current system-prompt contract.
    /// Per-library failures only log and never block startup.
    fn startup_maintenance(&self) {
        if let Err(error) = self.storage.reap_interrupted_runs() {
            tracing::warn!(%error, "could not reap runs interrupted by the previous process");
        }
        match self.storage.list_libraries() {
            Ok(libraries) => {
                for library in libraries {
                    if let Err(error) = self.storage.reconcile_staging(&library.id) {
                        tracing::warn!(library_id = %library.id, %error, "staging reconcile failed");
                    }
                    if let Err(error) = crate::bootstrap::write_skills(Path::new(&library.root)) {
                        tracing::warn!(library_id = %library.id, %error, "skills and contract refresh failed");
                    }
                }
            }
            Err(error) => {
                tracing::warn!(%error, "startup maintenance skipped: cannot list libraries")
            }
        }
    }

    /// All content libraries known to the control plane, oldest first.
    pub fn list_libraries(&self) -> Result<Vec<Library>, AppError> {
        self.storage.list_libraries()
    }

    pub async fn create_library(&self, request: CreateLibraryRequest) -> Result<Library, AppError> {
        let library = self.storage.create_library(&request)?;
        if let Err(error) = self.bootstrap_library(&library).await {
            tracing::error!(library_id = %library.id, error = %error, "library bootstrap failed");
            self.storage
                .discard_on_failure(&library.id, "library bootstrap");
            return Err(error);
        }
        Ok(library)
    }

    /// `library` accepts an exact id or a unique library name; it is
    /// resolved once here and every downstream call uses the resolved id.
    pub async fn submit_document(
        &self,
        library: &str,
        document: DocumentInput,
    ) -> Result<SubmitDocumentResponse, AppError> {
        if document.content.is_empty() {
            return Err(AppError::BadRequest(
                "document content cannot be empty".into(),
            ));
        }
        let library_id = self.storage.resolve_library(library)?.id;
        let stored = self.storage.store_document(
            &library_id,
            &document.filename,
            document.title.as_deref(),
            &document.content,
        )?;
        let job = self.storage.create_job(&library_id, JobKind::Ingest)?;
        if stored.duplicate {
            // A duplicate the wiki already compiles is a no-op skip. One no
            // node references is a document a job left behind by failing
            // before promotion: fall through and re-run ingestion on it —
            // dedupe would otherwise lose it forever.
            let root = self.storage.library_root(&library_id)?;
            if referenced_sources(&root.join("wiki"))?.contains(&stored.record.path) {
                self.storage
                    .update_job(&library_id, &job.job_id, JobState::Skipped, None, None)?;
                return Ok(SubmitDocumentResponse {
                    library_id,
                    job_id: job.job_id,
                    document_path: Some(stored.record.path),
                    duplicate: true,
                });
            }
        }

        let service = self.clone();
        let job_id = job.job_id.clone();
        let task_library_id = library_id.clone();
        let document_path = stored.record.path.clone();
        tokio::spawn(async move {
            if let Err(error) = service
                .process_ingest(&task_library_id, &job_id, stored)
                .await
            {
                tracing::error!(library_id = %task_library_id, job_id = %job_id, error = %error, "ingestion failed");
            }
        });

        Ok(SubmitDocumentResponse {
            library_id,
            job_id: job.job_id,
            document_path: Some(document_path),
            duplicate: false,
        })
    }

    /// `library` accepts an exact id or a unique library name.
    pub fn job_status(&self, library: &str, job_id: &str) -> Result<JobStatus, AppError> {
        let library_id = self.storage.resolve_library(library)?.id;
        self.storage.get_job(&library_id, job_id)
    }

    /// `library` accepts an exact id or a unique library name.
    pub async fn query(&self, library: &str, prompt: &str) -> Result<QueryResponse, AppError> {
        let prompt = prompt.trim();
        if prompt.is_empty() {
            return Err(AppError::BadRequest("prompt cannot be empty".into()));
        }
        let library_id = self.storage.resolve_library(library)?.id;
        let root = self.storage.library_root(&library_id)?;
        // Queries never queue behind ingests — readers keep the library
        // usable mid-ingestion — but they share the global session cap;
        // the run is only recorded once a permit is held.
        let _session_permit = self
            .sessions
            .acquire()
            .await
            .map_err(|_| AppError::Runtime("session semaphore closed".into()))?;
        let query_id = self.storage.record_query(&library_id)?;
        // The user question goes in unmodified: all policy (the answer
        // contract, citation discipline, library boundaries) reaches the
        // Agent through the system prompt from the library's generated
        // AGENTS.md contract.
        let request = AgentRunRequest {
            library_id: library_id.clone(),
            workdir: root.clone(),
            title: format!("Noema query {query_id}"),
            prompt: prompt.to_string(),
        };

        let result = match self.runtime.run_new_session(request).await {
            Ok(result) => result,
            Err(error) => return Err(self.record_query_failure(&query_id, error)),
        };
        self.storage.update_query(
            &query_id,
            JobState::Completed,
            Some(&result.session_id),
            None,
        )?;
        let (answer, references) = answer::present_answer(&root, &result.answer);
        Ok(QueryResponse {
            query_id,
            library_id,
            session_id: result.session_id,
            answer,
            references,
            tool_events: result.tool_events,
        })
    }

    /// Resolve one knowledge file for reading: `library` accepts an id or a
    /// unique name; `relative` must stay inside the library under `raw/` or
    /// `wiki/`. Canonicalization rejects symlink escapes; missing files and
    /// paths outside the tree are the same `FileNotFound` to the client.
    pub fn knowledge_file(&self, library: &str, relative: &str) -> Result<PathBuf, AppError> {
        let library_id = self.storage.resolve_library(library)?.id;
        if !safe_knowledge_path(relative) {
            return Err(AppError::BadRequest(format!(
                "not a raw/ or wiki/ knowledge path: {relative}"
            )));
        }
        let root = self.storage.library_root(&library_id)?;
        let not_found = || AppError::FileNotFound(relative.to_string());
        let canonical = root
            .join(relative)
            .canonicalize()
            .map_err(|_| not_found())?;
        let contained = root
            .canonicalize()
            .is_ok_and(|root| canonical.starts_with(root) && canonical.is_file());
        contained.then_some(canonical).ok_or_else(not_found)
    }

    /// Record a job failure and return the original error for propagation.
    /// A failure to record is logged rather than propagated: a broken
    /// database must never mask the actual cause from the caller.
    fn record_job_failure(
        &self,
        library_id: &str,
        job_id: &str,
        session_id: Option<&str>,
        original: AppError,
    ) -> AppError {
        if let Err(error) = self.storage.update_job(
            library_id,
            job_id,
            JobState::Failed,
            session_id,
            Some(&original.to_string()),
        ) {
            tracing::warn!(library_id = %library_id, job_id = %job_id, %error, "failed to record job failure");
        }
        original
    }

    /// Same as [`record_job_failure`] for query runs, wrapping the original
    /// error so the HTTP layer reports it as a query failure.
    fn record_query_failure(&self, query_id: &str, original: AppError) -> AppError {
        if let Err(error) = self.storage.update_query(
            query_id,
            JobState::Failed,
            None,
            Some(&original.to_string()),
        ) {
            tracing::warn!(query_id = %query_id, %error, "failed to record query failure");
        }
        AppError::QueryFailed(original.to_string())
    }

    async fn process_ingest(
        &self,
        library_id: &str,
        job_id: &str,
        stored: StoredDocument,
    ) -> Result<(), AppError> {
        // One ingest at a time per library: promotion replaces wiki/,
        // reviews/ and the graph wholesale, so two overlapping ingests
        // would each promote over the other's nodes. The waiter stays
        // `queued` (visible via job status) until it holds both the
        // library lock and a session permit; because it prepares its
        // staging only then, the incremental check below sees the
        // predecessor's graph and takes the --update path. Ingests on
        // different libraries never contend here.
        let _ingest_guard = self.ingest_lock(library_id).lock_owned().await;
        // Global admission control: bound how many OpenCode sessions run
        // at once across all libraries.
        let _session_permit = self
            .sessions
            .acquire()
            .await
            .map_err(|_| AppError::Runtime("session semaphore closed".into()))?;

        self.storage
            .update_job(library_id, job_id, JobState::Running, None, None)?;
        let (staging, baseline) = match self.storage.prepare_staging(library_id, job_id) {
            Ok(prepared) => prepared,
            Err(error) => return Err(self.record_job_failure(library_id, job_id, None, error)),
        };
        let incremental = staging.join("graphify-out/graph.json").is_file();
        let extras =
            match uncompiled_documents(&self.storage, library_id, &staging, &stored.record.path) {
                Ok(extras) => extras,
                Err(error) => {
                    return Err(self.record_job_failure(library_id, job_id, None, error));
                }
            };
        let request = AgentRunRequest {
            library_id: library_id.into(),
            workdir: staging,
            title: format!("Noema ingestion {job_id}"),
            prompt: format_ingest_task(&stored.record.path, job_id, incremental, &extras),
        };
        let result = match self.runtime.run_new_session(request).await {
            Ok(result) => result,
            Err(error) => return Err(self.record_job_failure(library_id, job_id, None, error)),
        };
        if let Err(error) = self
            .storage
            .validate_staging(library_id, job_id, &baseline)
            .and_then(|_| self.storage.promote_staging(library_id, job_id))
            .and_then(|_| self.storage.rebuild_index(library_id))
        {
            return Err(self.record_job_failure(
                library_id,
                job_id,
                Some(&result.session_id),
                error,
            ));
        }
        self.storage.update_job(
            library_id,
            job_id,
            JobState::Completed,
            Some(&result.session_id),
            None,
        )?;
        // Clean up only after the job is durably completed: a cleanup
        // failure must not strand an already-promoted job as `running` —
        // reconciliation removes leftover workspaces of completed jobs.
        if let Err(error) = self.storage.cleanup_staging(library_id, job_id) {
            tracing::warn!(library_id = %library_id, job_id = %job_id, %error, "staging cleanup failed");
        }
        // OpenCode writes this session's tombstone back into the staging
        // project directory minutes after the cleanup above, resurrecting
        // an empty skeleton; re-reconcile past that write-back window so a
        // long-running server stays clean (the startup sweep covers the
        // rest).
        let storage = self.storage.clone();
        let library_id = library_id.to_string();
        tokio::spawn(async move {
            tokio::time::sleep(STAGING_RECONCILE_DELAY).await;
            if let Err(error) = storage.reconcile_staging(&library_id) {
                tracing::warn!(library_id = %library_id, %error, "deferred staging reconcile failed");
            }
        });
        Ok(())
    }

    async fn bootstrap_library(&self, library: &Library) -> Result<(), AppError> {
        // Only the installer + skill refresh run on the blocking pool; the
        // control-plane insert and any rollback stay on the async task.
        let root = PathBuf::from(&library.root);
        tokio::task::spawn_blocking(move || crate::bootstrap::bootstrap(&root))
            .await
            .map_err(|error| AppError::Runtime(format!("bootstrap task aborted: {error}")))?
    }
}

/// The raw/ documents no wiki node references yet: uploads a predecessor job
/// left behind by failing before promotion. Nothing else would ever compile
/// them — dedupe skips their resubmission and later jobs' prompts name only
/// the newly submitted document — so the current session compiles them too.
fn uncompiled_documents(
    storage: &Storage,
    library_id: &str,
    staging: &Path,
    current: &str,
) -> Result<Vec<String>, AppError> {
    let referenced = referenced_sources(&staging.join("wiki"))?;
    Ok(storage
        .list_documents(library_id)?
        .into_iter()
        .map(|document| document.path)
        .filter(|path| path != current && !referenced.contains(path))
        .collect())
}

/// The ingest task message (user role): job-specific facts only. All policy
/// — the node contract, citation discipline, library boundaries — lives in
/// the library's generated `AGENTS.md` contract and reaches the Agent
/// through OpenCode's system prompt.
fn format_ingest_task(
    source_path: &str,
    job_id: &str,
    incremental: bool,
    extras: &[String],
) -> String {
    let graphify_step = if incremental {
        "当前 staging 已有 graphify-out/graph.json，因此必须调用 OpenCode 的 `skill` 工具加载上游 graphify Skill，并严格执行 `/graphify . --update`。这是上游 Skill 的文档/文本增量流程；不要替换成裸 `graphify update .`，后者主要只更新代码 AST。"
    } else {
        "当前 staging 尚无 graphify-out/graph.json，因此必须调用 OpenCode 的 `skill` 工具加载上游 graphify Skill，并严格执行 `/graphify .` 完整首次建图流程。"
    };
    let extras_step = if extras.is_empty() {
        String::new()
    } else {
        format!(
            "此外，以下源文档已入库但尚无任何 wiki 节点引用（此前的摄入在落盘前失败）：{}。请将它们与本源文档一视同仁地编译为 wiki 节点并登记 index.md。\n\n",
            extras.join("、")
        )
    };
    format!(
        "摄入任务 {job_id}。先阅读 purpose.md 和 schema.md，再阅读源文档 {source_path}；节点落盘后同步更新 index.md。项目根目录已经提供 `.graphifyignore`，上游 graphify 因此只检测 `raw/` 和 `wiki/` 下的 Markdown/TXT。\n\n{extras_step}{graphify_step} 让它在当前 staging 根目录生成或更新标准的 `graphify-out/graph.json`、`GRAPH_REPORT.md` 和 HTML 等产物。语义抽取使用当前 OpenCode Agent 能力；不要改用需要外部 API key 的 headless `graphify extract`。\n"
    )
}
