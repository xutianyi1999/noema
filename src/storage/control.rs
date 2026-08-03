//! Control-plane persistence: libraries, ingestion jobs and query runs in
//! control.sqlite.

use std::{
    fs,
    path::{Path, PathBuf},
};

use chrono::Utc;
use rusqlite::{OptionalExtension, params};
use unicode_normalization::UnicodeNormalization;
use uuid::Uuid;

use super::{Storage, layout, parse_timestamp_or_now};
use crate::{
    error::AppError,
    models::{CreateLibraryRequest, JobKind, JobState, JobStatus, Library},
};

impl Storage {
    pub fn create_library(&self, request: &CreateLibraryRequest) -> Result<Library, AppError> {
        // The name is the identity: it becomes the library id and the
        // directory name verbatim, so names are unique and path-safe.
        let name: String = request.name.trim().nfc().collect();
        validate_library_id(&name)?;
        let taken = self
            .db()?
            .query_row(
                "SELECT 1 FROM libraries WHERE name = ?1",
                params![name],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if taken {
            return Err(AppError::Conflict(format!(
                "a library named {name} already exists"
            )));
        }

        let id = name.clone();
        let root = self.root.join("libraries").join(&id);
        let now = Utc::now();

        if let Err(error) = layout::scaffold_library(&root, &name) {
            let _ = fs::remove_dir_all(&root);
            return Err(error);
        }

        let library = Library {
            id: id.clone(),
            name,
            description: request.description.clone(),
            root: root.display().to_string(),
            created_at: now,
        };

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
        match result {
            Ok(_) => Ok(library),
            // Lost the name race against a concurrent creator (the pre-check
            // above is not atomic with this INSERT): its row and directory
            // are the real ones now. Report the conflict WITHOUT removing
            // the directory — that would destroy the winner's library.
            Err(rusqlite::Error::SqliteFailure(failure, _))
                if failure.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                Err(AppError::Conflict(format!(
                    "a library named {} already exists",
                    library.name
                )))
            }
            Err(error) => {
                let _ = fs::remove_dir_all(&root);
                Err(error.into())
            }
        }
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
        let expected_root =
            canonicalize_library_path(&self.root.join("libraries").join(library_id), library_id)?;
        let actual_root = canonicalize_library_path(Path::new(&library.root), library_id)?;
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

    /// Fail every job and query run a previous server process left in
    /// `running`: this process has run nothing yet, so they were interrupted
    /// by its shutdown or crash, and nothing else would ever transition them
    /// out of that state. Runs at startup, before staging reconciliation.
    pub fn reap_interrupted_runs(&self) -> Result<(), AppError> {
        const INTERRUPTED: &str = "interrupted by server restart";
        let now = Utc::now().to_rfc3339();
        let connection = self.db()?;
        connection.execute(
            "UPDATE jobs SET status = ?1, error = ?2, updated_at = ?3 WHERE status = ?4",
            params![JobState::Failed, INTERRUPTED, now, JobState::Running],
        )?;
        connection.execute(
            "UPDATE query_runs SET status = ?1, error = ?2, updated_at = ?3 WHERE status = ?4",
            params![JobState::Failed, INTERRUPTED, now, JobState::Running],
        )?;
        Ok(())
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
    /// unambiguous name. New libraries use their name verbatim as the id and
    /// names are unique, so for them the two match; the name fallback still
    /// covers legacy libraries whose ids carry a random suffix. CLI and HTTP
    /// callers may pass either; storage internals always receive the
    /// resolved id.
    pub fn resolve_library(&self, selector: &str) -> Result<Library, AppError> {
        let selector: String = selector.trim().nfc().collect();
        let libraries = self.list_libraries()?;
        if let Some(library) = libraries.iter().find(|item| item.id == selector) {
            return Ok(library.clone());
        }
        let by_name: Vec<&Library> = libraries
            .iter()
            .filter(|item| item.name == selector)
            .collect();
        match by_name.as_slice() {
            [] => Err(AppError::LibraryNotFound(selector)),
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

/// Library ids are the library names themselves (legacy ids with a random
/// suffix remain valid), so the id character set is whatever is safe as a
/// single directory-name component: non-empty, within the filesystem's
/// 255-byte limit, no path separators, control characters, dot-names or
/// double quotes (the id travels inside a quoted Content-Disposition
/// `filename` parameter on export). Unicode letters and digits — CJK names
/// included — are allowed; URLs travel percent-encoded and SQLite keys are
/// plain TEXT.
fn validate_library_id(value: &str) -> Result<(), AppError> {
    let invalid = value.is_empty()
        || value.len() > 255
        || matches!(value, "." | "..")
        || value
            .chars()
            .any(|character| character.is_control() || matches!(character, '/' | '\\' | '"'));
    if invalid {
        return Err(AppError::BadRequest("invalid library_id".into()));
    }
    Ok(())
}

/// Canonicalize one library path, reporting a library whose directory is
/// gone as `LibraryNotFound` (404) rather than a raw I/O error (500): a
/// library without its tree is functionally absent.
fn canonicalize_library_path(path: &Path, library_id: &str) -> Result<PathBuf, AppError> {
    fs::canonicalize(path).map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound => AppError::LibraryNotFound(library_id.into()),
        _ => AppError::Io(error),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{CreateLibraryRequest, JobKind};

    fn fixture() -> (tempfile::TempDir, Storage) {
        let tmp = tempfile::tempdir().unwrap();
        let storage = Storage::open(tmp.path()).unwrap();
        (tmp, storage)
    }

    #[test]
    fn create_library_conflict_never_touches_the_existing_library() {
        let (_tmp, storage) = fixture();
        let original = storage
            .create_library(&CreateLibraryRequest {
                name: "法规库".into(),
                description: None,
            })
            .unwrap();
        // Force the INSERT constraint path past the name pre-check: rename
        // the existing row, then recreate under the original name — the id
        // (and therefore the directory) collides on the PRIMARY KEY, which
        // is exactly where the concurrent-creator race lands.
        storage
            .db()
            .unwrap()
            .execute(
                "UPDATE libraries SET name = 'renamed' WHERE id = '法规库'",
                [],
            )
            .unwrap();
        let conflict = storage.create_library(&CreateLibraryRequest {
            name: "法规库".into(),
            description: None,
        });
        assert!(
            matches!(conflict, Err(AppError::Conflict(_))),
            "{conflict:?}"
        );
        // The winner's directory survived the loser's rollback.
        let library = storage.get_library("法规库").unwrap();
        assert_eq!(library.root, original.root);
        assert!(
            PathBuf::from(&library.root)
                .join("library.sqlite")
                .is_file()
        );
    }

    #[test]
    fn reap_interrupted_runs_fails_rows_left_running_by_a_dead_process() {
        let (_tmp, storage) = fixture();
        let library = storage
            .create_library(&CreateLibraryRequest {
                name: "回收库".into(),
                description: None,
            })
            .unwrap();
        let interrupted = storage.create_job(&library.id, JobKind::Ingest).unwrap();
        storage
            .update_job(
                &library.id,
                &interrupted.job_id,
                JobState::Running,
                None,
                None,
            )
            .unwrap();
        let finished = storage.create_job(&library.id, JobKind::Ingest).unwrap();
        storage
            .update_job(
                &library.id,
                &finished.job_id,
                JobState::Completed,
                None,
                None,
            )
            .unwrap();
        let query_id = storage.record_query(&library.id).unwrap();

        storage.reap_interrupted_runs().unwrap();

        let interrupted = storage.get_job(&library.id, &interrupted.job_id).unwrap();
        assert_eq!(interrupted.status, JobState::Failed);
        assert_eq!(
            interrupted.error.as_deref(),
            Some("interrupted by server restart")
        );
        assert_eq!(
            storage
                .get_job(&library.id, &finished.job_id)
                .unwrap()
                .status,
            JobState::Completed
        );
        let (status, error): (JobState, Option<String>) = storage
            .db()
            .unwrap()
            .query_row(
                "SELECT status, error FROM query_runs WHERE id = ?1",
                params![query_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(status, JobState::Failed);
        assert_eq!(error.as_deref(), Some("interrupted by server restart"));
    }
}
