//! Control-plane persistence: libraries, ingestion jobs and query runs in
//! control.sqlite.

use std::{fs, path::PathBuf};

use chrono::Utc;
use rusqlite::{OptionalExtension, params};
use uuid::Uuid;

use super::{Storage, layout, parse_timestamp_or_now};
use crate::{
    error::AppError,
    models::{CreateLibraryRequest, JobKind, JobState, JobStatus, Library},
};

impl Storage {
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

        layout::scaffold_library(&root, name)?;

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
                        created_at: parse_timestamp_or_now(&created_at),
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
                    created_at: parse_timestamp_or_now(&created_at),
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(libraries)
    }

    /// Resolve a protocol-layer library selector: an exact id first, then an
    /// unambiguous name. CLI and HTTP callers may pass either; storage
    /// internals always receive the resolved id.
    pub fn resolve_library(&self, selector: &str) -> Result<Library, AppError> {
        let libraries = self.list_libraries()?;
        if let Some(library) = libraries.iter().find(|item| item.id == selector) {
            return Ok(library.clone());
        }
        let by_name: Vec<&Library> = libraries
            .iter()
            .filter(|item| item.name == selector)
            .collect();
        match by_name.as_slice() {
            [] => Err(AppError::LibraryNotFound(selector.into())),
            [library] => Ok((*library).clone()),
            many => {
                let ids = many
                    .iter()
                    .map(|item| item.id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                Err(AppError::BadRequest(format!(
                    "multiple libraries are named {selector:?}; select one by id: {ids}"
                )))
            }
        }
    }

    pub fn create_job(&self, library_id: &str, kind: JobKind) -> Result<JobStatus, AppError> {
        let _ = self.get_library(library_id)?;
        let now = Utc::now();
        let job = JobStatus {
            job_id: Uuid::new_v4().to_string(),
            library_id: library_id.into(),
            kind,
            status: JobState::Queued,
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
        status: JobState,
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
            "INSERT INTO query_runs (id, library_id, status, session_id, error, created_at, updated_at) VALUES (?1, ?2, ?3, NULL, NULL, ?4, ?4)",
            params![id, library_id, JobState::Running, now],
        )?;
        Ok(id)
    }

    pub fn update_query(
        &self,
        query_id: &str,
        status: JobState,
        session_id: Option<&str>,
        error: Option<&str>,
    ) -> Result<(), AppError> {
        self.db()?.execute(
            "UPDATE query_runs SET status = ?1, session_id = COALESCE(?2, session_id), error = ?3, updated_at = ?4 WHERE id = ?5",
            params![status, session_id, error, Utc::now().to_rfc3339(), query_id],
        )?;
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
        created_at: parse_timestamp_or_now(&created_at),
        updated_at: parse_timestamp_or_now(&updated_at),
    })
}

fn slugify(value: &str) -> String {
    // Unicode-aware (CJK names transliterate to pinyin instead of
    // collapsing to the empty-string fallback).
    let slug = slug::slugify(value);
    if slug.is_empty() {
        return "library".into();
    }
    slug.chars()
        .take(48)
        .collect::<String>()
        .trim_matches('-')
        .into()
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
