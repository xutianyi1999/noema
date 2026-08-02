use std::{
    fs,
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
};

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use sha2::{Digest, Sha256};
use uuid::Uuid;
use walkdir::WalkDir;

use crate::{
    error::AppError,
    models::{CreateLibraryRequest, JobStatus, Library, Reference},
};

#[derive(Clone)]
pub struct Storage {
    root: Arc<PathBuf>,
    control: Arc<Mutex<Connection>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DocumentRecord {
    pub id: String,
    pub filename: String,
    pub title: String,
    pub path: String,
    pub sha256: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct StoredDocument {
    pub record: DocumentRecord,
    pub duplicate: bool,
}

impl Storage {
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, AppError> {
        let root = root.into();
        fs::create_dir_all(root.join("libraries"))?;
        fs::create_dir_all(root.join("jobs"))?;
        let root = fs::canonicalize(root)?;
        let connection = Connection::open(root.join("control.sqlite"))?;
        connection.execute_batch(
            "
            PRAGMA foreign_keys = ON;
            CREATE TABLE IF NOT EXISTS libraries (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                description TEXT,
                root TEXT NOT NULL UNIQUE,
                created_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS jobs (
                id TEXT PRIMARY KEY,
                library_id TEXT NOT NULL,
                kind TEXT NOT NULL,
                status TEXT NOT NULL,
                error TEXT,
                session_id TEXT,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS query_runs (
                id TEXT PRIMARY KEY,
                library_id TEXT NOT NULL,
                status TEXT NOT NULL,
                session_id TEXT,
                error TEXT,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );
            ",
        )?;

        Ok(Self {
            root: Arc::new(root),
            control: Arc::new(Mutex::new(connection)),
        })
    }

    pub fn root(&self) -> &Path {
        self.root.as_path()
    }

    fn db(&self) -> Result<MutexGuard<'_, Connection>, AppError> {
        self.control
            .lock()
            .map_err(|_| AppError::Storage("control database lock poisoned".into()))
    }

    pub fn create_library(&self, request: &CreateLibraryRequest) -> Result<Library, AppError> {
        let name = request.name.trim();
        if name.is_empty() {
            return Err(AppError::BadRequest("library name cannot be empty".into()));
        }

        let id = format!(
            "{}-{}",
            slugify(name),
            &Uuid::new_v4().simple().to_string()[..12]
        );
        let root = self.root.join("libraries").join(&id);
        let now = Utc::now();
        let library = Library {
            id: id.clone(),
            name: name.into(),
            description: request.description.clone(),
            root: root.display().to_string(),
            created_at: now,
        };

        for directory in [
            ".opencode",
            "raw",
            "wiki",
            "graph",
            "index",
            "reviews",
            "staging",
            "graphify-out",
        ] {
            fs::create_dir_all(root.join(directory))?;
        }
        write_atomic(&root.join("purpose.md"), default_purpose(name).as_bytes())?;
        write_atomic(&root.join("schema.md"), default_schema().as_bytes())?;
        write_atomic(&root.join("index.md"), default_index(name).as_bytes())?;
        write_atomic(
            &root.join(".graphifyignore"),
            graphify_scope_ignore().as_bytes(),
        )?;
        write_atomic(&root.join("manifest.json"), b"{\n  \"documents\": []\n}\n")?;
        init_library_db(&root.join("library.sqlite"))?;

        let result = self.db()?.execute(
            "INSERT INTO libraries (id, name, description, root, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                library.id,
                library.name,
                library.description,
                library.root,
                library.created_at.to_rfc3339(),
            ],
        );
        if let Err(error) = result {
            let _ = fs::remove_dir_all(&root);
            return Err(error.into());
        }

        Ok(library)
    }

    pub fn get_library(&self, library_id: &str) -> Result<Library, AppError> {
        validate_library_id(library_id)?;
        let row = self
            .db()?
            .query_row(
                "SELECT id, name, description, root, created_at FROM libraries WHERE id = ?1",
                params![library_id],
                |row| {
                    let created_at: String = row.get(4)?;
                    Ok(Library {
                        id: row.get(0)?,
                        name: row.get(1)?,
                        description: row.get(2)?,
                        root: row.get(3)?,
                        created_at: DateTime::parse_from_rfc3339(&created_at)
                            .map(|value| value.with_timezone(&Utc))
                            .unwrap_or_else(|_| Utc::now()),
                    })
                },
            )
            .optional()?;
        let mut library = row.ok_or_else(|| AppError::LibraryNotFound(library_id.into()))?;
        let expected_root = fs::canonicalize(self.root.join("libraries").join(library_id))?;
        let actual_root = fs::canonicalize(&library.root)?;
        if actual_root != expected_root {
            return Err(AppError::Storage(format!(
                "library root is outside its registered content-library path: {library_id}"
            )));
        }
        library.root = actual_root.display().to_string();
        Ok(library)
    }

    pub fn library_root(&self, library_id: &str) -> Result<PathBuf, AppError> {
        Ok(PathBuf::from(self.get_library(library_id)?.root))
    }

    pub fn list_libraries(&self) -> Result<Vec<Library>, AppError> {
        let connection = self.db()?;
        let mut statement = connection.prepare(
            "SELECT id, name, description, root, created_at FROM libraries ORDER BY created_at",
        )?;
        let libraries = statement
            .query_map([], |row| {
                let created_at: String = row.get(4)?;
                Ok(Library {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    description: row.get(2)?,
                    root: row.get(3)?,
                    created_at: DateTime::parse_from_rfc3339(&created_at)
                        .map(|value| value.with_timezone(&Utc))
                        .unwrap_or_else(|_| Utc::now()),
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(libraries)
    }

    pub fn store_document(
        &self,
        library_id: &str,
        filename: &str,
        title: Option<&str>,
        content: &str,
    ) -> Result<StoredDocument, AppError> {
        let root = self.library_root(library_id)?;
        validate_filename(filename)?;
        let mut hasher = Sha256::new();
        hasher.update(content.as_bytes());
        let sha256 = hex::encode(hasher.finalize());
        let now = Utc::now();
        let connection = Connection::open(root.join("library.sqlite"))?;

        if let Some(existing) = connection
            .query_row(
                "SELECT id, filename, title, path, sha256, created_at FROM documents WHERE sha256 = ?1",
                params![sha256],
                document_from_row,
            )
            .optional()?
        {
            return Ok(StoredDocument {
                record: existing,
                duplicate: true,
            });
        }

        let id = Uuid::new_v4().to_string();
        let safe_name = format!("{}-{}", &sha256[..12], filename);
        let relative_path = format!("raw/{safe_name}");
        write_atomic(&root.join(&relative_path), content.as_bytes())?;
        let record = DocumentRecord {
            id,
            filename: filename.into(),
            title: title
                .filter(|value| !value.trim().is_empty())
                .map(ToOwned::to_owned)
                .unwrap_or_else(|| {
                    Path::new(filename)
                        .file_stem()
                        .and_then(|value| value.to_str())
                        .unwrap_or(filename)
                        .to_string()
                }),
            path: relative_path,
            sha256,
            created_at: now,
        };
        connection.execute(
            "INSERT INTO documents (id, filename, title, path, sha256, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                record.id,
                record.filename,
                record.title,
                record.path,
                record.sha256,
                record.created_at.to_rfc3339(),
            ],
        )?;
        self.write_manifest(library_id)?;
        Ok(StoredDocument {
            record,
            duplicate: false,
        })
    }

    pub fn create_job(&self, library_id: &str, kind: &str) -> Result<JobStatus, AppError> {
        let _ = self.get_library(library_id)?;
        let now = Utc::now();
        let job = JobStatus {
            job_id: Uuid::new_v4().to_string(),
            library_id: library_id.into(),
            kind: kind.into(),
            status: "queued".into(),
            error: None,
            session_id: None,
            created_at: now,
            updated_at: now,
        };
        self.db()?.execute(
            "INSERT INTO jobs (id, library_id, kind, status, error, session_id, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                job.job_id,
                job.library_id,
                job.kind,
                job.status,
                job.error,
                job.session_id,
                job.created_at.to_rfc3339(),
                job.updated_at.to_rfc3339(),
            ],
        )?;
        Ok(job)
    }

    pub fn update_job(
        &self,
        library_id: &str,
        job_id: &str,
        status: &str,
        session_id: Option<&str>,
        error: Option<&str>,
    ) -> Result<JobStatus, AppError> {
        let _ = self.get_job(library_id, job_id)?;
        let now = Utc::now();
        self.db()?.execute(
            "UPDATE jobs SET status = ?1, error = ?2, session_id = COALESCE(?3, session_id), updated_at = ?4 WHERE id = ?5 AND library_id = ?6",
            params![status, error, session_id, now.to_rfc3339(), job_id, library_id],
        )?;
        self.get_job(library_id, job_id)
    }

    pub fn get_job(&self, library_id: &str, job_id: &str) -> Result<JobStatus, AppError> {
        let row = self
            .db()?
            .query_row(
                "SELECT id, library_id, kind, status, error, session_id, created_at, updated_at FROM jobs WHERE id = ?1 AND library_id = ?2",
                params![job_id, library_id],
                job_from_row,
            )
            .optional()?;
        row.ok_or_else(|| AppError::JobNotFound(job_id.into()))
    }

    pub fn record_query(&self, library_id: &str) -> Result<String, AppError> {
        let _ = self.get_library(library_id)?;
        let id = Uuid::new_v4().to_string();
        let now = Utc::now().to_rfc3339();
        self.db()?.execute(
            "INSERT INTO query_runs (id, library_id, status, session_id, error, created_at, updated_at) VALUES (?1, ?2, 'running', NULL, NULL, ?3, ?3)",
            params![id, library_id, now],
        )?;
        Ok(id)
    }

    pub fn update_query(
        &self,
        query_id: &str,
        status: &str,
        session_id: Option<&str>,
        error: Option<&str>,
    ) -> Result<(), AppError> {
        self.db()?.execute(
            "UPDATE query_runs SET status = ?1, session_id = COALESCE(?2, session_id), error = ?3, updated_at = ?4 WHERE id = ?5",
            params![status, session_id, error, Utc::now().to_rfc3339(), query_id],
        )?;
        Ok(())
    }

    pub fn write_manifest(&self, library_id: &str) -> Result<(), AppError> {
        let root = self.library_root(library_id)?;
        let connection = Connection::open(root.join("library.sqlite"))?;
        let mut statement = connection.prepare(
            "SELECT id, filename, title, path, sha256, created_at FROM documents ORDER BY created_at",
        )?;
        let documents = statement
            .query_map([], document_from_row)?
            .collect::<Result<Vec<_>, _>>()?;
        let manifest = serde_json::json!({ "documents": documents });
        write_atomic(
            &root.join("manifest.json"),
            serde_json::to_vec_pretty(&manifest)?.as_slice(),
        )?;
        Ok(())
    }

    pub fn rebuild_index(&self, library_id: &str) -> Result<(), AppError> {
        let root = self.library_root(library_id)?;
        let mut output = String::from("# Knowledge Library Index\n\n");
        output.push_str("This file is generated by Noema and is a navigation aid.\n\n");
        for (label, directory, extensions) in [
            ("Sources", "raw", &["md", "txt"] as &[&str]),
            ("Knowledge nodes", "wiki", &["md"] as &[&str]),
        ] {
            output.push_str(&format!("## {label}\n\n"));
            let directory_path = root.join(directory);
            let mut paths = Vec::new();
            if directory_path.exists() {
                for entry in WalkDir::new(&directory_path).follow_links(false) {
                    let entry = entry.map_err(|error| AppError::Storage(error.to_string()))?;
                    if entry.file_type().is_file()
                        && entry
                            .path()
                            .extension()
                            .and_then(|value| value.to_str())
                            .is_some_and(|extension| extensions.contains(&extension))
                    {
                        paths.push(entry.path().to_path_buf());
                    }
                }
            }
            paths.sort();
            for path in paths {
                let relative = path
                    .strip_prefix(&root)
                    .map_err(|error| AppError::Storage(error.to_string()))?
                    .display();
                output.push_str(&format!("- `{relative}`\n"));
            }
            output.push('\n');
        }
        write_atomic(&root.join("index.md"), output.as_bytes())?;
        rebuild_content_fts(&root)?;
        self.write_manifest(library_id)
    }

    pub fn prepare_staging(&self, library_id: &str, job_id: &str) -> Result<PathBuf, AppError> {
        let root = self.library_root(library_id)?;
        let staging = root.join("staging").join(job_id);
        if staging.exists() {
            fs::remove_dir_all(&staging)?;
        }
        fs::create_dir_all(&staging)?;
        for entry in [
            ".opencode",
            ".graphifyignore",
            "AGENTS.md",
            "purpose.md",
            "schema.md",
            "index.md",
            "raw",
            "wiki",
            "graph",
            "index",
            "reviews",
            "manifest.json",
            "graphify-out",
        ] {
            let source = root.join(entry);
            let destination = staging.join(entry);
            if source.exists() {
                copy_path(&source, &destination)?;
            }
        }
        Ok(staging)
    }

    pub fn promote_staging(&self, library_id: &str, job_id: &str) -> Result<(), AppError> {
        let root = self.library_root(library_id)?;
        let staging = root.join("staging").join(job_id);
        if !staging.exists() {
            return Err(AppError::Storage(format!(
                "missing staging directory {job_id}"
            )));
        }
        for entry in ["wiki", "graph", "index", "reviews", "graphify-out"] {
            let source = staging.join(entry);
            let destination = root.join(entry);
            if source.exists() {
                if destination.exists() {
                    fs::remove_dir_all(&destination)?;
                }
                copy_path(&source, &destination)?;
            }
        }
        for entry in ["index.md", "manifest.json"] {
            let source = staging.join(entry);
            if source.exists() {
                fs::copy(source, root.join(entry))?;
            }
        }
        Ok(())
    }

    pub fn validate_staging(&self, library_id: &str, job_id: &str) -> Result<(), AppError> {
        let root = self.library_root(library_id)?;
        let staging = root.join("staging").join(job_id);
        if !staging.is_dir() {
            return Err(AppError::Storage(format!(
                "missing staging directory {job_id}"
            )));
        }

        for entry in WalkDir::new(&staging).follow_links(false) {
            let entry = entry.map_err(|error| AppError::Storage(error.to_string()))?;
            if entry.file_type().is_symlink() {
                let relative = entry
                    .path()
                    .strip_prefix(&staging)
                    .map_err(|error| AppError::Storage(error.to_string()))?;
                // OpenCode has full permissions. Package managers used by the
                // installed graphify/OpenCode project may create links under
                // .opencode/node_modules; that runtime tree is never promoted.
                if relative.starts_with(".opencode") {
                    continue;
                }
                return Err(AppError::Storage(format!(
                    "staging contains a symbolic link: {}",
                    entry.path().display()
                )));
            }
        }

        let allowed = [
            ".opencode",
            ".graphifyignore",
            "AGENTS.md",
            "purpose.md",
            "schema.md",
            "index.md",
            "raw",
            "wiki",
            "graph",
            "index",
            "reviews",
            "manifest.json",
            "graphify-out",
        ];
        for entry in fs::read_dir(&staging)? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !allowed.iter().any(|item| *item == name) {
                return Err(AppError::Storage(format!(
                    "staging contains an unauthorized top-level path: {name}"
                )));
            }
        }

        // The Agent is allowed to install/update runtime dependencies in the
        // staging project's .opencode directory. It is intentionally not part
        // of the promoted output, so only knowledge inputs and the library
        // contract are compared against the baseline.
        for relative in [".graphifyignore", "raw", "purpose.md", "schema.md"] {
            if !same_tree(&root.join(relative), &staging.join(relative))? {
                return Err(AppError::Storage(format!(
                    "staging modified protected path: {relative}"
                )));
            }
        }
        if staging.join("library.sqlite").exists() {
            return Err(AppError::Storage(
                "staging cannot contain library.sqlite".into(),
            ));
        }
        validate_wiki_nodes(&staging.join("wiki"))
    }

    pub fn cleanup_staging(&self, library_id: &str, job_id: &str) -> Result<(), AppError> {
        let staging = self.library_root(library_id)?.join("staging").join(job_id);
        if staging.exists() {
            fs::remove_dir_all(staging)?;
        }
        Ok(())
    }

    pub fn discard_library(&self, library_id: &str) -> Result<(), AppError> {
        let library = self.get_library(library_id)?;
        let root = PathBuf::from(&library.root);
        if root.exists() {
            fs::remove_dir_all(root)?;
        }
        self.db()?.execute(
            "DELETE FROM jobs WHERE library_id = ?1",
            params![library_id],
        )?;
        self.db()?.execute(
            "DELETE FROM query_runs WHERE library_id = ?1",
            params![library_id],
        )?;
        self.db()?
            .execute("DELETE FROM libraries WHERE id = ?1", params![library_id])?;
        Ok(())
    }

    pub fn references_from_answer(
        &self,
        library_id: &str,
        answer: &str,
    ) -> Result<Vec<Reference>, AppError> {
        let root = self.library_root(library_id)?;
        let mut references = Vec::new();
        // Real answers cite paths with locators and mixed punctuation, e.g.
        // `raw/abc-source.md:5-7,15；wiki/concept.md:24）`. Take the maximal
        // run of path-safe characters at every `raw/` / `wiki/` occurrence
        // instead of splitting on whitespace.
        let mut occurrences: Vec<(usize, &str)> = ["raw/", "wiki/"]
            .iter()
            .flat_map(|prefix| {
                answer
                    .match_indices(prefix)
                    .map(|(start, _)| (start, *prefix))
            })
            .collect();
        occurrences.sort_unstable_by_key(|(start, _)| *start);
        for (start, prefix) in occurrences {
            let Some(candidate) = reference_candidate(answer, start) else {
                continue;
            };
            let path = root.join(&candidate);
            if is_safe_reference(&candidate, prefix)
                && path.is_file()
                && !references
                    .iter()
                    .any(|item: &Reference| item.source == candidate)
            {
                let title = Path::new(&candidate)
                    .file_stem()
                    .and_then(|value| value.to_str())
                    .unwrap_or(&candidate)
                    .to_string();
                let node = if candidate.starts_with("raw/") {
                    let stem = Path::new(&candidate)
                        .file_stem()
                        .and_then(|value| value.to_str())
                        .unwrap_or_default();
                    let node = format!("wiki/{stem}.md");
                    root.join(&node).is_file().then_some(node)
                } else {
                    None
                };
                references.push(Reference {
                    title,
                    source: candidate,
                    node,
                });
            }
        }
        Ok(references)
    }
}

/// Extracts a `raw/…` or `wiki/…` file path starting at `start`, stopping at
/// the first character outside `[A-Za-z0-9/._-]` and dropping any trailing
/// dots. Line/column locators (`:24`, `:5-7,15`) and surrounding punctuation
/// are therefore excluded. Returns `None` unless the run ends in an allowed
/// knowledge-file extension.
fn reference_candidate(text: &str, start: usize) -> Option<String> {
    let run: String = text[start..]
        .chars()
        .take_while(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '/' | '.' | '_' | '-')
        })
        .collect();
    let candidate = run.trim_end_matches('.');
    (candidate.ends_with(".md") || candidate.ends_with(".txt")).then(|| candidate.to_string())
}

fn init_library_db(path: &Path) -> Result<(), AppError> {
    let connection = Connection::open(path)?;
    connection.execute_batch(
        "
        PRAGMA foreign_keys = ON;
        CREATE TABLE IF NOT EXISTS documents (
            id TEXT PRIMARY KEY,
            filename TEXT NOT NULL,
            title TEXT NOT NULL,
            path TEXT NOT NULL UNIQUE,
            sha256 TEXT NOT NULL UNIQUE,
            created_at TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS nodes (
            node_id TEXT PRIMARY KEY,
            path TEXT NOT NULL UNIQUE,
            canonical_name TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );
        CREATE VIRTUAL TABLE IF NOT EXISTS content_fts USING fts5(path, title, body);
        ",
    )?;
    Ok(())
}

fn document_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<DocumentRecord> {
    let created_at: String = row.get(5)?;
    Ok(DocumentRecord {
        id: row.get(0)?,
        filename: row.get(1)?,
        title: row.get(2)?,
        path: row.get(3)?,
        sha256: row.get(4)?,
        created_at: DateTime::parse_from_rfc3339(&created_at)
            .map(|value| value.with_timezone(&Utc))
            .unwrap_or_else(|_| Utc::now()),
    })
}

fn job_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<JobStatus> {
    let created_at: String = row.get(6)?;
    let updated_at: String = row.get(7)?;
    Ok(JobStatus {
        job_id: row.get(0)?,
        library_id: row.get(1)?,
        kind: row.get(2)?,
        status: row.get(3)?,
        error: row.get(4)?,
        session_id: row.get(5)?,
        created_at: DateTime::parse_from_rfc3339(&created_at)
            .map(|value| value.with_timezone(&Utc))
            .unwrap_or_else(|_| Utc::now()),
        updated_at: DateTime::parse_from_rfc3339(&updated_at)
            .map(|value| value.with_timezone(&Utc))
            .unwrap_or_else(|_| Utc::now()),
    })
}

pub(crate) fn copy_path(source: &Path, destination: &Path) -> Result<(), AppError> {
    if source.is_dir() {
        fs::create_dir_all(destination)?;
        for entry in WalkDir::new(source).follow_links(false) {
            let entry = entry.map_err(|error| AppError::Storage(error.to_string()))?;
            let relative = entry
                .path()
                .strip_prefix(source)
                .map_err(|error| AppError::Storage(error.to_string()))?;
            let target = destination.join(relative);
            if entry.file_type().is_dir() {
                fs::create_dir_all(target)?;
            } else if entry.file_type().is_file() {
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::copy(entry.path(), target)?;
            }
        }
    } else if source.is_file() {
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(source, destination)?;
    }
    Ok(())
}

fn same_tree(left: &Path, right: &Path) -> Result<bool, AppError> {
    if !left.exists() || !right.exists() {
        return Ok(left.exists() == right.exists());
    }
    if left.is_file() || right.is_file() {
        return Ok(left.is_file() && right.is_file() && fs::read(left)? == fs::read(right)?);
    }
    if !left.is_dir() || !right.is_dir() {
        return Ok(false);
    }

    let left_entries = tree_entries(left)?;
    let right_entries = tree_entries(right)?;
    if left_entries != right_entries {
        return Ok(false);
    }
    for (relative, is_dir) in left_entries {
        if !is_dir && fs::read(left.join(&relative))? != fs::read(right.join(&relative))? {
            return Ok(false);
        }
    }
    Ok(true)
}

fn tree_entries(root: &Path) -> Result<Vec<(PathBuf, bool)>, AppError> {
    let mut entries = Vec::new();
    for entry in WalkDir::new(root).follow_links(false).min_depth(1) {
        let entry = entry.map_err(|error| AppError::Storage(error.to_string()))?;
        let relative = entry
            .path()
            .strip_prefix(root)
            .map_err(|error| AppError::Storage(error.to_string()))?
            .to_path_buf();
        entries.push((relative, entry.file_type().is_dir()));
    }
    entries.sort();
    Ok(entries)
}

fn validate_wiki_nodes(root: &Path) -> Result<(), AppError> {
    if !root.exists() {
        return Ok(());
    }
    let required = [
        "node_id:",
        "canonical_name:",
        "kind:",
        "sources:",
        "relations:",
        "claim_type:",
        "confidence:",
        "created_at:",
        "updated_at:",
    ];
    for entry in WalkDir::new(root).follow_links(false) {
        let entry = entry.map_err(|error| AppError::Storage(error.to_string()))?;
        if !entry.file_type().is_file()
            || entry
                .path()
                .extension()
                .and_then(|value| value.to_str())
                .is_none_or(|value| !value.eq_ignore_ascii_case("md"))
        {
            continue;
        }
        let content = fs::read_to_string(entry.path())?;
        let mut lines = content.lines();
        if lines.next() != Some("---") {
            return Err(AppError::Storage(format!(
                "wiki node is missing YAML frontmatter: {}",
                entry.path().display()
            )));
        }
        let mut frontmatter = String::new();
        let mut closed = false;
        for line in lines {
            if line == "---" {
                closed = true;
                break;
            }
            frontmatter.push_str(line);
            frontmatter.push('\n');
        }
        if !closed
            || required.iter().any(|key| {
                !frontmatter
                    .lines()
                    .any(|line| line.trim_start().starts_with(key))
            })
        {
            return Err(AppError::Storage(format!(
                "wiki node has incomplete YAML frontmatter: {}",
                entry.path().display()
            )));
        }
    }
    Ok(())
}

fn rebuild_content_fts(root: &Path) -> Result<(), AppError> {
    let connection = Connection::open(root.join("library.sqlite"))?;
    connection.execute("DELETE FROM content_fts", [])?;

    for directory in ["raw", "wiki"] {
        let directory_path = root.join(directory);
        if !directory_path.exists() {
            continue;
        }
        for entry in WalkDir::new(&directory_path).follow_links(false) {
            let entry = entry.map_err(|error| AppError::Storage(error.to_string()))?;
            if !entry.file_type().is_file() {
                continue;
            }
            let extension = entry
                .path()
                .extension()
                .and_then(|value| value.to_str())
                .unwrap_or_default();
            let allowed = if directory == "raw" {
                extension.eq_ignore_ascii_case("md") || extension.eq_ignore_ascii_case("txt")
            } else {
                extension.eq_ignore_ascii_case("md")
            };
            if !allowed {
                continue;
            }
            let body = fs::read_to_string(entry.path())?;
            let relative = entry
                .path()
                .strip_prefix(root)
                .map_err(|error| AppError::Storage(error.to_string()))?
                .display()
                .to_string();
            let title = entry
                .path()
                .file_stem()
                .and_then(|value| value.to_str())
                .unwrap_or_default();
            connection.execute(
                "INSERT INTO content_fts (path, title, body) VALUES (?1, ?2, ?3)",
                params![relative, title, body],
            )?;
        }
    }
    Ok(())
}

fn write_atomic(path: &Path, content: &[u8]) -> Result<(), AppError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension(format!("tmp-{}", Uuid::new_v4()));
    fs::write(&temporary, content)?;
    fs::rename(temporary, path)?;
    Ok(())
}

fn validate_library_id(value: &str) -> Result<(), AppError> {
    if value.is_empty()
        || value.len() > 80
        || !value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-' || byte == b'_'
        })
    {
        return Err(AppError::BadRequest("invalid library_id".into()));
    }
    Ok(())
}

fn validate_filename(value: &str) -> Result<(), AppError> {
    let path = Path::new(value);
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default();
    if value.is_empty()
        || value.contains('/')
        || value.contains('\\')
        || value == "."
        || value == ".."
        || !matches!(extension.to_ascii_lowercase().as_str(), "md" | "txt")
    {
        return Err(AppError::BadRequest(
            "filename must be a single .md or .txt filename".into(),
        ));
    }
    Ok(())
}

fn slugify(value: &str) -> String {
    let mut result = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() {
            result.push(byte.to_ascii_lowercase() as char);
        } else if (byte == b'-' || byte == b'_') && !result.ends_with('-') {
            result.push(byte as char);
        } else if !result.ends_with('-') {
            result.push('-');
        }
    }
    let value = result.trim_matches('-');
    if value.is_empty() {
        "library".into()
    } else {
        value
            .chars()
            .take(48)
            .collect::<String>()
            .trim_matches('-')
            .into()
    }
}

fn is_safe_reference(candidate: &str, prefix: &str) -> bool {
    candidate.starts_with(prefix)
        && !Path::new(candidate).is_absolute()
        && Path::new(candidate).components().all(|component| {
            !matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
}

fn default_purpose(name: &str) -> String {
    format!(
        "# {name}\n\nDefine the purpose, scope, key questions, terminology, and update policy for this content library.\n"
    )
}

fn graphify_scope_ignore() -> &'static str {
    "# Noema graphify input boundary: only text sources and compiled nodes.\n*\n!raw/\n!raw/**\n!wiki/\n!wiki/**\n"
}

fn default_schema() -> String {
    "# Knowledge Schema\n\n\
     知识节点契约（详见 knowledge-compiler Skill）：frontmatter 恰好包含 9 个键 —— \
     node_id（库内稳定标识）、canonical_name、kind（concept|entity|process|decision|issue）、\
     sources（raw/ 下的 path + locator）、relations（depends_on / related_to / opposite_to）、\
     claim_type（observed|summarized|inferred|unresolved）、confidence（0.0–1.0）、\
     created_at、updated_at（RFC-3339）；不要添加额外键。\n\n\
     正文包含 6 个小节：定义、证据/推理、示例或反例、局限性、\
     RAG Version（100–300 字的高密度压缩摘要，不是版本变更记录）、\
     引用（raw/... 或 wiki/... 相对路径）。未解决的声明放入 reviews/。\n"
        .into()
}

fn default_index(name: &str) -> String {
    format!("# {name}\n\nNo documents have been ingested yet.\n")
}
