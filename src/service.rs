use std::{path::Path, process::Stdio, sync::Arc};

use tokio::process::Command;

use crate::{
    config::Config,
    error::AppError,
    models::{
        CreateLibraryRequest, DocumentInput, HealthResponse, JobStatus, Library, QueryResponse,
        SubmitDocumentResponse,
    },
    runtime::{AgentRunRequest, OpenCodeAgent, OpenCodeRuntime},
    storage::{Storage, StoredDocument},
};

pub struct AppService<R: OpenCodeRuntime> {
    pub storage: Storage,
    config: Arc<Config>,
    runtime: Arc<R>,
}

impl<R: OpenCodeRuntime> Clone for AppService<R> {
    fn clone(&self) -> Self {
        Self {
            storage: self.storage.clone(),
            config: self.config.clone(),
            runtime: self.runtime.clone(),
        }
    }
}

impl AppService<OpenCodeAgent> {
    pub fn new(config: Config) -> Result<Self, AppError> {
        let config = Arc::new(config);
        let storage = Storage::open(config.data_dir.clone())?;
        let runtime = Arc::new(OpenCodeAgent::new(config.clone()));
        Ok(Self {
            storage,
            config,
            runtime,
        })
    }
}

impl<R: OpenCodeRuntime> AppService<R> {
    pub fn with_runtime(config: Config, runtime: Arc<R>) -> Result<Self, AppError> {
        let config = Arc::new(config);
        let storage = Storage::open(config.data_dir.clone())?;
        Ok(Self {
            storage,
            config,
            runtime,
        })
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

    pub async fn submit_document(
        &self,
        library_id: &str,
        document: DocumentInput,
    ) -> Result<SubmitDocumentResponse, AppError> {
        if document.content.is_empty() {
            return Err(AppError::BadRequest(
                "document content cannot be empty".into(),
            ));
        }
        let stored = self.storage.store_document(
            library_id,
            &document.filename,
            document.title.as_deref(),
            &document.content,
        )?;
        let job = self.storage.create_job(library_id, "ingest")?;
        if stored.duplicate {
            self.storage
                .update_job(library_id, &job.job_id, "skipped", None, None)?;
            return Ok(SubmitDocumentResponse {
                library_id: library_id.to_string(),
                job_id: job.job_id,
                document_path: Some(stored.record.path),
                duplicate: true,
            });
        }

        let service = self.clone();
        let library_id = library_id.to_string();
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

    pub fn job_status(&self, library_id: &str, job_id: &str) -> Result<JobStatus, AppError> {
        self.storage.get_job(library_id, job_id)
    }

    pub async fn query(&self, library_id: &str, prompt: &str) -> Result<QueryResponse, AppError> {
        let prompt = prompt.trim();
        if prompt.is_empty() {
            return Err(AppError::BadRequest("prompt cannot be empty".into()));
        }
        let root = self.storage.library_root(library_id)?;
        let query_id = self.storage.record_query(library_id)?;
        let request = AgentRunRequest {
            library_id: library_id.into(),
            workdir: root,
            title: format!("Noema query {query_id}"),
            prompt: format_query_prompt(prompt),
        };

        let result = match self.runtime.run_new_session(request).await {
            Ok(result) => result,
            Err(error) => {
                self.storage
                    .update_query(&query_id, "failed", None, Some(&error.to_string()))?;
                return Err(AppError::QueryFailed(error.to_string()));
            }
        };
        self.storage
            .update_query(&query_id, "completed", Some(&result.session_id), None)?;
        let references = self
            .storage
            .references_from_answer(library_id, &result.answer)?;
        Ok(QueryResponse {
            query_id,
            library_id: library_id.into(),
            session_id: result.session_id,
            answer: result.answer,
            references,
            tool_events: result.tool_events,
        })
    }

    async fn process_ingest(
        &self,
        library_id: &str,
        job_id: &str,
        stored: StoredDocument,
    ) -> Result<(), AppError> {
        self.storage
            .update_job(library_id, job_id, "running", None, None)?;
        let staging = match self.storage.prepare_staging(library_id, job_id) {
            Ok(path) => path,
            Err(error) => {
                self.storage.update_job(
                    library_id,
                    job_id,
                    "failed",
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
                    "failed",
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
                "failed",
                Some(&result.session_id),
                Some(&error.to_string()),
            )?;
            return Err(error);
        }
        self.storage.cleanup_staging(library_id, job_id)?;
        self.storage.update_job(
            library_id,
            job_id,
            "completed",
            Some(&result.session_id),
            None,
        )?;
        Ok(())
    }

    async fn bootstrap_library(&self, library: &Library) -> Result<(), AppError> {
        let root = Path::new(&library.root);
        let output = Command::new("graphify")
            .args(["install", "--platform", "opencode", "--project"])
            .current_dir(root)
            .stdin(Stdio::null())
            .output()
            .await
            .map_err(|error| {
                AppError::Runtime(format!("unable to run graphify installer: {error}"))
            })?;
        if !output.status.success() {
            return Err(AppError::Runtime(format!(
                "graphify installer failed: {}",
                String::from_utf8_lossy(&output.stderr)
            )));
        }
        for (relative, contents) in crate::snapshot::skill_files() {
            let path = root.join(".opencode").join("skills").join(relative);
            if let Some(parent) = path.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            tokio::fs::write(path, contents).await?;
        }
        Ok(())
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
