use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use tokio::sync::Semaphore;
use unicode_normalization::UnicodeNormalization;

use crate::{
    answer,
    config::Config,
    error::AppError,
    models::{
        CreateLibraryRequest, DocumentInput, HealthResponse, JobKind, JobState, JobStatus, Library,
        QueryResponse, SubmitDocumentsResponse, SubmittedDocument,
    },
    references::{contained_file, safe_knowledge_path},
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

// Every field is cheaply cloneable on its own; the manual impl keeps `R`
// itself out of the bound (`Arc<R>` clones regardless of `R`), which a
// derived `Clone` would add.
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
        let runtime = Arc::new(OpenCodeAgent::new(config.clone()));
        Self::from_parts(config, runtime)
    }
}

impl<R: OpenCodeRuntime> AppService<R> {
    pub fn with_runtime(config: Config, runtime: Arc<R>) -> Result<Self, AppError> {
        Self::from_parts(Arc::new(config), runtime)
    }

    fn from_parts(config: Arc<Config>, runtime: Arc<R>) -> Result<Self, AppError> {
        if config.max_sessions == 0 {
            return Err(AppError::BadRequest(
                "max_sessions must be at least 1".into(),
            ));
        }
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
        HealthResponse {
            status: "ok",
            data_dir: detailed.then(|| self.storage.root().display().to_string()),
            opencode_url: detailed.then(|| self.config.opencode_url.clone()),
            configured_model: detailed.then(|| self.config.opencode_model.clone()),
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
    /// A library whose creation died mid-installer has no graphify artifacts
    /// yet; for it the full bootstrap is re-run so startup converges every
    /// library to a working state. Per-library failures only log and never
    /// block startup.
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
                    if let Err(error) = self.storage.reconcile_documents(&library.id) {
                        tracing::warn!(library_id = %library.id, %error, "document reconcile failed");
                    }
                    let root = Path::new(&library.root);
                    let refreshed = if crate::bootstrap::graphify_installed(root) {
                        crate::bootstrap::write_skills(root)
                    } else {
                        crate::bootstrap::bootstrap(root)
                    };
                    if let Err(error) = refreshed {
                        tracing::warn!(library_id = %library.id, %error, "library refresh failed");
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

    /// The one submission entry, for one document or many: every document
    /// is stored, and all non-skipped ones are compiled together in ONE
    /// ingestion job — one staging workspace, one agent session.
    /// Same-library ingests are serialized by the ingest lock and promotion
    /// swaps the compiled tree wholesale, so N separate jobs would only
    /// queue up N full pipelines without any concurrency gain, while one
    /// session can merge concepts that span the documents.
    ///
    /// `library` accepts an exact id or a unique library name; it is
    /// resolved once here and every downstream call uses the resolved id.
    pub async fn submit_documents(
        &self,
        library: &str,
        documents: Vec<DocumentInput>,
    ) -> Result<SubmitDocumentsResponse, AppError> {
        if documents.is_empty() {
            return Err(AppError::BadRequest("documents cannot be empty".into()));
        }
        for document in &documents {
            if document.content.is_empty() {
                return Err(AppError::BadRequest(format!(
                    "document {} content cannot be empty",
                    document.filename
                )));
            }
        }
        let library_id = self.storage.resolve_library(library)?.id;
        // Normalization within one submission: the NFC-normalized filename
        // is the identity (same as store_document). A repeated name with
        // identical content collapses to one entry; different content is a
        // conflict.
        let mut kept: Vec<DocumentInput> = Vec::new();
        let mut seen: HashMap<String, String> = HashMap::new();
        for document in documents {
            let filename: String = document.filename.nfc().collect();
            match seen.get(&filename) {
                Some(first) if *first != document.content => {
                    return Err(AppError::Conflict(format!(
                        "a document named {filename} appears more than once in the submission with different content"
                    )));
                }
                Some(_) => continue,
                None => {}
            }
            seen.insert(filename.clone(), document.content.clone());
            kept.push(DocumentInput {
                filename,
                content: document.content,
                title: document.title,
            });
        }
        // Store loop. A mid-submission error (e.g. a name conflict against
        // content already in the library) fails the whole call; documents
        // stored earlier stay in raw/ uncompiled and self-heal via the
        // extras mechanism on the next ingest — the same residue state a
        // job that failed before promotion leaves behind.
        let mut stored_documents: Vec<(DocumentInput, StoredDocument)> = Vec::new();
        for document in kept {
            let stored = self.storage.store_document(
                &library_id,
                &document.filename,
                document.title.as_deref(),
                &document.content,
            )?;
            stored_documents.push((document, stored));
        }
        // One wiki walk for the whole submission (per-path walks would be
        // O(documents × wiki)).
        let referenced = referenced_sources(&self.storage.library_root(&library_id)?.join("wiki"))?;
        let mut entries: Vec<SubmittedDocument> = Vec::new();
        let mut to_ingest: Vec<StoredDocument> = Vec::new();
        let mut seen_paths: HashSet<String> = HashSet::new();
        for (document, stored) in stored_documents {
            // A duplicate the wiki already compiles is a no-op skip. One no
            // node references is a document a job left behind by failing
            // before promotion: fall through and re-run ingestion on it —
            // dedupe would otherwise lose it forever. Decided before the job
            // row exists so no failure below can strand a forever-queued job.
            let skip = stored.duplicate && referenced.contains(&stored.record.path);
            entries.push(SubmittedDocument {
                filename: document.filename,
                document_path: stored.record.path.clone(),
                duplicate: stored.duplicate,
                skipped: skip,
            });
            // Identical content submitted under two names shares one stored
            // record: the same file must not be named twice in the prompt.
            if !skip && seen_paths.insert(stored.record.path.clone()) {
                to_ingest.push(stored);
            }
        }
        let job = self.storage.create_job(&library_id, JobKind::Ingest)?;
        if to_ingest.is_empty() {
            // Record the skip durably; a failure here must mark the job
            // failed rather than leave it queued forever.
            if let Err(error) =
                self.storage
                    .update_job(&library_id, &job.job_id, JobState::Skipped, None, None)
            {
                return Err(self.record_job_failure(&library_id, &job.job_id, None, error));
            }
            return Ok(SubmitDocumentsResponse {
                library_id,
                job_id: job.job_id,
                documents: entries,
            });
        }

        let service = self.clone();
        let job_id = job.job_id.clone();
        let task_library_id = library_id.clone();
        tokio::spawn(async move {
            if let Err(error) = service
                .process_ingest(&task_library_id, &job_id, to_ingest)
                .await
            {
                tracing::error!(library_id = %task_library_id, job_id = %job_id, error = %error, "ingestion failed");
            }
        });

        Ok(SubmitDocumentsResponse {
            library_id,
            job_id: job.job_id,
            documents: entries,
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
            .map_err(|_| sessions_closed())?;
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
        // The run succeeded; a failure to record that must not discard the
        // answer (the run row simply stays `running` until the startup reap
        // picks it up). Same discipline as [`record_job_failure`]: a broken
        // database must never mask successful work.
        if let Err(error) = self.storage.update_query(
            &query_id,
            JobState::Completed,
            Some(&result.session_id),
            None,
        ) {
            tracing::warn!(query_id = %query_id, %error, "failed to record query completion");
        }
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
    /// `wiki/`. Canonicalization rejects symlink escapes; missing files,
    /// malformed shapes and paths outside the tree are all the same
    /// `FileNotFound` to the client, so the path policy itself is never
    /// revealed by the status code.
    pub fn knowledge_file(&self, library: &str, relative: &str) -> Result<PathBuf, AppError> {
        let library_id = self.storage.resolve_library(library)?.id;
        let root = self.storage.library_root(&library_id)?;
        if safe_knowledge_path(relative)
            && let Some(canonical) = contained_file(&root, relative)
        {
            return Ok(canonical);
        }
        Err(AppError::FileNotFound(relative.to_string()))
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
        stored: Vec<StoredDocument>,
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
            .map_err(|_| sessions_closed())?;

        // A predecessor job may have compiled some of these documents in the
        // meantime: it names the uncompiled documents it finds, and the same
        // content resubmitted mid-run spawns a second job for it. Re-checked
        // here under the library lock so the late job skips instead of
        // redoing a whole ingest session over already-compiled documents.
        let referenced =
            match referenced_sources(&self.storage.library_root(library_id)?.join("wiki")) {
                Ok(referenced) => referenced,
                // Like every other failure below, record it on the job row: a
                // bare `?` would leave the job stranded as `queued`.
                Err(error) => return Err(self.record_job_failure(library_id, job_id, None, error)),
            };
        let pending: Vec<StoredDocument> = stored
            .into_iter()
            .filter(|stored| !referenced.contains(&stored.record.path))
            .collect();
        if pending.is_empty() {
            if let Err(error) =
                self.storage
                    .update_job(library_id, job_id, JobState::Skipped, None, None)
            {
                return Err(self.record_job_failure(library_id, job_id, None, error));
            }
            return Ok(());
        }

        if let Err(error) =
            self.storage
                .update_job(library_id, job_id, JobState::Running, None, None)
        {
            return Err(self.record_job_failure(library_id, job_id, None, error));
        }
        let (staging, baseline) = match self.storage.prepare_staging(library_id, job_id) {
            Ok(prepared) => prepared,
            Err(error) => return Err(self.record_job_failure(library_id, job_id, None, error)),
        };
        let incremental = staging.join("graphify-out/graph.json").is_file();
        let paths: Vec<String> = pending
            .iter()
            .map(|stored| stored.record.path.clone())
            .collect();
        let extras = match uncompiled_documents(&self.storage, library_id, &staging, &paths) {
            Ok(extras) => extras,
            Err(error) => {
                return Err(self.record_job_failure(library_id, job_id, None, error));
            }
        };
        let request = AgentRunRequest {
            library_id: library_id.into(),
            workdir: staging,
            title: format!("Noema ingestion {job_id}"),
            prompt: format_ingest_task(&paths, job_id, incremental, &extras),
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
        if let Err(error) = self.storage.update_job(
            library_id,
            job_id,
            JobState::Completed,
            Some(&result.session_id),
            None,
        ) {
            // The promotion is durable — only the status write failed. Drop
            // the staging copy anyway (unlike a genuine ingest failure
            // there is nothing left to inspect) and report the job failed
            // with the true cause instead of leaving it `running`.
            self.cleanup_staging(library_id, job_id);
            return Err(self.record_job_failure(
                library_id,
                job_id,
                Some(&result.session_id),
                error,
            ));
        }
        // Clean up only after the job is durably completed: a cleanup
        // failure must not strand an already-promoted job as `running` —
        // reconciliation removes leftover workspaces of completed jobs.
        self.cleanup_staging(library_id, job_id);
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

    /// Drop one job's staging workspace; a failure only logs — the startup
    /// reconciliation sweep eventually removes whatever is left behind.
    fn cleanup_staging(&self, library_id: &str, job_id: &str) {
        if let Err(error) = self.storage.cleanup_staging(library_id, job_id) {
            tracing::warn!(library_id = %library_id, job_id = %job_id, %error, "staging cleanup failed");
        }
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
/// the newly submitted documents — so the current session compiles them too.
fn uncompiled_documents(
    storage: &Storage,
    library_id: &str,
    staging: &Path,
    current: &[String],
) -> Result<Vec<String>, AppError> {
    let referenced = referenced_sources(&staging.join("wiki"))?;
    Ok(storage
        .list_documents(library_id)?
        .into_iter()
        .map(|document| document.path)
        .filter(|path| {
            !current.contains(path)
                && !referenced.contains(path)
                // Submissions are not gated by the ingest lock, so a document
                // submitted after this staging copy was prepared is already
                // in the database but absent from staging raw/: its own
                // queued job will compile it; naming it here would ask the
                // session to read a file its workspace does not contain.
                && staging.join(path).is_file()
        })
        .collect())
}

/// The global session semaphore is only closed at shutdown; a refused
/// permit means the server is winding down and admits no new work.
fn sessions_closed() -> AppError {
    AppError::Runtime("session semaphore closed".into())
}

/// The ingest task message (user role): job-specific facts only. All policy
/// — the node contract, citation discipline, library boundaries — lives in
/// the library's generated `AGENTS.md` contract and reaches the Agent
/// through OpenCode's system prompt.
fn format_ingest_task(
    source_paths: &[String],
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
    let reading_step = if source_paths.len() == 1 {
        format!(
            "再阅读源文档 {}；节点落盘后同步更新 index.md。",
            source_paths[0]
        )
    } else {
        format!(
            "再依次阅读以下源文档：{}；多个文档中描述同一概念的内容合并为一个节点，并在该节点的 sources 中列出全部来源。节点落盘后同步更新 index.md。",
            source_paths.join("、")
        )
    };
    format!(
        "摄入任务 {job_id}。先阅读 purpose.md 和 schema.md，{reading_step}项目根目录已经提供 `.graphifyignore`，上游 graphify 因此只检测 `raw/` 和 `wiki/` 下的 Markdown/TXT。\n\n{extras_step}{graphify_step} 让它在当前 staging 根目录生成或更新标准的 `graphify-out/graph.json`、`GRAPH_REPORT.md` 和 HTML 等产物。语义抽取使用当前 OpenCode Agent 能力；不要改用需要外部 API key 的 headless `graphify extract`。\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extras_never_name_documents_absent_from_the_staging_copy() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = Storage::open(tmp.path()).unwrap();
        let library = storage
            .create_library(&CreateLibraryRequest {
                name: "暂存库".into(),
                description: None,
            })
            .unwrap();
        storage
            .store_document(&library.id, "a.md", None, "content a")
            .unwrap();
        storage
            .store_document(&library.id, "b.md", None, "content b")
            .unwrap();
        // Staging copy taken before b.md landed: its DB row must not leak
        // into the extras list — the session would be told to compile a
        // file its workspace does not contain.
        let staging = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(staging.path().join("raw")).unwrap();
        std::fs::write(staging.path().join("raw/a.md"), "content a").unwrap();
        let extras = uncompiled_documents(
            &storage,
            &library.id,
            staging.path(),
            &["raw/a.md".to_string()],
        )
        .unwrap();
        assert!(extras.is_empty(), "{extras:?}");
    }

    #[test]
    fn uncompiled_documents_excludes_every_path_of_the_current_batch() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = Storage::open(tmp.path()).unwrap();
        let library = storage
            .create_library(&CreateLibraryRequest {
                name: "批量库".into(),
                description: None,
            })
            .unwrap();
        for (name, content) in [
            ("a.md", "content a"),
            ("b.md", "content b"),
            ("c.md", "content c"),
        ] {
            storage
                .store_document(&library.id, name, None, content)
                .unwrap();
        }
        let staging = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(staging.path().join("raw")).unwrap();
        for name in ["a.md", "b.md", "c.md"] {
            std::fs::write(
                staging.path().join(format!("raw/{name}")),
                format!("content {}", &name[..1]),
            )
            .unwrap();
        }
        let extras = uncompiled_documents(
            &storage,
            &library.id,
            staging.path(),
            &["raw/a.md".to_string(), "raw/b.md".to_string()],
        )
        .unwrap();
        assert_eq!(extras, vec!["raw/c.md".to_string()], "{extras:?}");
    }

    /// The single-document prompt is the long-deployed contract; the batch
    /// refactor must not drift a single byte of it.
    #[test]
    fn format_ingest_task_single_path_is_byte_stable() {
        let prompt = format_ingest_task(&["raw/a.md".to_string()], "job-1", false, &[]);
        assert_eq!(
            prompt,
            "摄入任务 job-1。先阅读 purpose.md 和 schema.md，再阅读源文档 raw/a.md；节点落盘后同步更新 index.md。项目根目录已经提供 `.graphifyignore`，上游 graphify 因此只检测 `raw/` 和 `wiki/` 下的 Markdown/TXT。\n\n当前 staging 尚无 graphify-out/graph.json，因此必须调用 OpenCode 的 `skill` 工具加载上游 graphify Skill，并严格执行 `/graphify .` 完整首次建图流程。 让它在当前 staging 根目录生成或更新标准的 `graphify-out/graph.json`、`GRAPH_REPORT.md` 和 HTML 等产物。语义抽取使用当前 OpenCode Agent 能力；不要改用需要外部 API key 的 headless `graphify extract`。\n"
        );
    }

    #[test]
    fn format_ingest_task_multiple_paths_names_each_and_merges_concepts() {
        let prompt = format_ingest_task(
            &["raw/a.md".to_string(), "raw/b.md".to_string()],
            "job-2",
            true,
            &[],
        );
        assert!(
            prompt.contains("再依次阅读以下源文档：raw/a.md、raw/b.md"),
            "{prompt}"
        );
        assert!(prompt.contains("合并为一个节点"), "{prompt}");
        assert!(prompt.contains("/graphify . --update"), "{prompt}");
    }
}
