use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use tokio::sync::Semaphore;

use crate::{
    config::Config,
    error::AppError,
    models::{
        CreateLibraryRequest, DocumentInput, HealthResponse, JobKind, JobState, JobStatus, Library,
        QueryResponse, SubmitDocumentResponse,
    },
    references,
    runtime::{AgentRunRequest, OpenCodeAgent, OpenCodeRuntime},
    storage::{Storage, StoredDocument},
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
        service.reconcile_staging();
        Ok(service)
    }
}

impl<R: OpenCodeRuntime> AppService<R> {
    pub fn with_runtime(config: Config, runtime: Arc<R>) -> Result<Self, AppError> {
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
        service.reconcile_staging();
        Ok(service)
    }

    pub fn health(&self) -> HealthResponse {
        HealthResponse {
            status: "ok",
            data_dir: self.config.data_dir.display().to_string(),
            opencode_url: self.config.opencode_url.clone(),
            configured_model: self.config.opencode_model.clone(),
        }
    }

    /// Data directory this service manages (control plane + library roots).
    pub fn data_dir(&self) -> &Path {
        &self.config.data_dir
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

    /// Drop leftover staging workspaces whose jobs no longer need them (see
    /// [`Storage::reconcile_staging`]). Runs at startup so residue from
    /// previous runs — including directories OpenCode's session write-back
    /// resurrected — converges away; per-library failures only log and
    /// never block startup.
    fn reconcile_staging(&self) {
        match self.storage.list_libraries() {
            Ok(libraries) => {
                for library in libraries {
                    if let Err(error) = self.storage.reconcile_staging(&library.id) {
                        tracing::warn!(library_id = %library.id, %error, "staging reconcile failed");
                    }
                }
            }
            Err(error) => {
                tracing::warn!(%error, "staging reconcile skipped: cannot list libraries")
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
            if let Err(cleanup_error) = self.storage.discard_library(&library.id) {
                tracing::error!(
                    library_id = %library.id,
                    error = %cleanup_error,
                    "failed to roll back library bootstrap"
                );
            }
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
            self.storage
                .update_job(&library_id, &job.job_id, JobState::Skipped, None, None)?;
            return Ok(SubmitDocumentResponse {
                library_id,
                job_id: job.job_id,
                document_path: Some(stored.record.path),
                duplicate: true,
            });
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
        let request = AgentRunRequest {
            library_id: library_id.clone(),
            workdir: root.clone(),
            title: format!("Noema query {query_id}"),
            prompt: format_query_prompt(prompt),
        };

        let result = match self.runtime.run_new_session(request).await {
            Ok(result) => result,
            Err(error) => {
                self.storage.update_query(
                    &query_id,
                    JobState::Failed,
                    None,
                    Some(&error.to_string()),
                )?;
                return Err(AppError::QueryFailed(error.to_string()));
            }
        };
        self.storage.update_query(
            &query_id,
            JobState::Completed,
            Some(&result.session_id),
            None,
        )?;
        let cited = references::extract_references(&root, &result.answer);
        Ok(QueryResponse {
            query_id,
            library_id,
            session_id: result.session_id,
            answer: result.answer,
            references: cited,
            tool_events: result.tool_events,
        })
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
        let staging = match self.storage.prepare_staging(library_id, job_id) {
            Ok(path) => path,
            Err(error) => {
                self.storage.update_job(
                    library_id,
                    job_id,
                    JobState::Failed,
                    None,
                    Some(&error.to_string()),
                )?;
                return Err(error);
            }
        };
        let incremental = staging.join("graphify-out/graph.json").is_file();
        let request = AgentRunRequest {
            library_id: library_id.into(),
            workdir: staging,
            title: format!("Noema ingestion {job_id}"),
            prompt: format_ingest_prompt(&stored.record.path, job_id, incremental),
        };
        let result = match self.runtime.run_new_session(request).await {
            Ok(result) => result,
            Err(error) => {
                self.storage.update_job(
                    library_id,
                    job_id,
                    JobState::Failed,
                    None,
                    Some(&error.to_string()),
                )?;
                return Err(error);
            }
        };
        if let Err(error) = self
            .storage
            .validate_staging(library_id, job_id)
            .and_then(|_| self.storage.promote_staging(library_id, job_id))
            .and_then(|_| self.storage.rebuild_index(library_id))
        {
            self.storage.update_job(
                library_id,
                job_id,
                JobState::Failed,
                Some(&result.session_id),
                Some(&error.to_string()),
            )?;
            return Err(error);
        }
        self.storage.cleanup_staging(library_id, job_id)?;
        self.storage.update_job(
            library_id,
            job_id,
            JobState::Completed,
            Some(&result.session_id),
            None,
        )?;
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

fn format_query_prompt(prompt: &str) -> String {
    format!(
        "你是 Noema 内容库查询 Agent。只能在当前内容库项目内工作。先阅读 purpose.md 和 schema.md，再阅读 index.md；摘要优先——先读相关 wiki 节点的定义和 RAG Version 压缩摘要，不足时再读完整节点与 raw/ 原文；涉及关系问题时按需运行 graphify query。不要写入、编辑或删除文件，也不要访问项目外路径。只依据内容库证据回答用户的自然语言问题，简洁说明推理过程，并为每个事实性结论引用相对路径，例如 raw/example.md 或 wiki/concept.md。最终答案用 <noema-answer> 与 </noema-answer> 包裹，标记之间只放答案本身。\n\n用户问题：\n{prompt}"
    )
}

fn format_ingest_prompt(source_path: &str, job_id: &str, incremental: bool) -> String {
    let graphify_step = if incremental {
        "当前 staging 已有 graphify-out/graph.json，因此必须调用 OpenCode 的 `skill` 工具加载上游 graphify Skill，并严格执行 `/graphify . --update`。这是上游 Skill 的文档/文本增量流程；不要替换成裸 `graphify update .`，后者主要只更新代码 AST。"
    } else {
        "当前 staging 尚无 graphify-out/graph.json，因此必须调用 OpenCode 的 `skill` 工具加载上游 graphify Skill，并严格执行 `/graphify .` 完整首次建图流程。"
    };
    format!(
        "你是 Noema 摄入任务 {job_id} 的知识编译 Agent。只能在当前暂存内容库项目内工作。先阅读 purpose.md 和 schema.md，再阅读 {source_path}；写入节点前先调用 OpenCode 的 `skill` 工具加载 knowledge-compiler Skill 并遵循其节点契约。创建节点前检查已有 wiki 节点，避免重复。创建或更新可追溯的知识节点：frontmatter 恰好包含契约定义的 9 个键，不要添加额外键；正文包含定义、证据/推理、示例或反例、局限性、RAG Version 和引用，其中 RAG Version 是节点 100–300 字的高密度压缩摘要（保留核心推理链，适合直接注入 LLM 上下文），不是版本变更记录。将未解决的冲突或低置信度关系放入 reviews/。\n\n{graphify_step} 项目根目录已经提供 `.graphifyignore`，上游 graphify 因此只检测 `raw/` 和 `wiki/` 下的 Markdown/TXT；让它在当前 staging 根目录生成或更新标准的 `graphify-out/graph.json`、`GRAPH_REPORT.md` 和 HTML 等产物。语义抽取使用当前 OpenCode Agent 能力；不要改用需要外部 API key 的 headless `graphify extract`。\n\n不要修改 raw 原文、.graphifyignore、.opencode、library.sqlite，也不要访问暂存项目外的路径。\n"
    )
}
