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
            return Err(library_name_conflict(&name));
        }

        let id = name.clone();
        let root = self.root.join("libraries").join(&id);
        let now = Utc::now();

        let library = Library {
            id: id.clone(),
            name,
            description: request.description.clone(),
            root: root.display().to_string(),
            created_at: now,
        };

        // Claim the name in the control plane BEFORE touching the
        // filesystem: the UNIQUE constraint arbitrates concurrent creators
        // atomically, so scaffolding only ever runs for the name's owner.
        // (Scaffolding first let a loser's scaffold failure delete the
        // directory the winner was using.)
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
            Err(rusqlite::Error::SqliteFailure(failure, _))
                if failure.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                return Err(library_name_conflict(&library.name));
            }
            Err(error) => return Err(error.into()),
            Ok(_) => {}
        }

        // This call owns the row, so on scaffold failure both the row and
        // the directory belong to it and can be rolled back safely.
        if let Err(error) = layout::scaffold_library(&root, &library.name) {
            let _ = fs::remove_dir_all(&root);
            let _ = self
                .db()?
                .execute("DELETE FROM libraries WHERE id = ?1", params![library.id]);
            return Err(error);
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
                library_from_row,
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

    /// Fail every run a previous server process left non-terminal: jobs
    /// still `running` died mid-session, jobs still `queued` died waiting
    /// for their library lock, and `running` query runs died the same way.
    /// This process has spawned nothing yet, so nothing else would ever
    /// transition them. Runs at startup, before staging reconciliation.
    pub fn reap_interrupted_runs(&self) -> Result<(), AppError> {
        const INTERRUPTED: &str = "interrupted by server restart";
        let now = Utc::now().to_rfc3339();
        let connection = self.db()?;
        connection.execute(
            "UPDATE jobs SET status = ?1, error = ?2, updated_at = ?3 WHERE status IN (?4, ?5)",
            params![
                JobState::Failed,
                INTERRUPTED,
                now,
                JobState::Running,
                JobState::Queued
            ],
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
        Ok(statement
            .query_map([], library_from_row)?
            .collect::<Result<Vec<_>, _>>()?)
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

    /// Whether `session_id` was returned by a completed query in `library_id`.
    /// Session ids are OpenCode capabilities, so never let a caller attach an
    /// arbitrary (or another library's) session to this library's workspace.
    pub fn has_completed_query_session(
        &self,
        library_id: &str,
        session_id: &str,
    ) -> Result<bool, AppError> {
        let exists = self
            .db()?
            .query_row(
                "SELECT 1 FROM query_runs WHERE library_id = ?1 AND session_id = ?2 AND status = ?3 LIMIT 1",
                params![library_id, session_id, JobState::Completed],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        Ok(exists)
    }

    /// Remove a library completely: directory tree and every control-plane
    /// row. Reads the row directly instead of [`get_library`] so a library
    /// whose tree is missing or partial (a failed creation, a botched
    /// rollback) can still be discarded instead of orphaning its rows
    /// forever. The row deletions run in one transaction; the schema has no
    /// foreign keys, so a crash mid-delete would otherwise leave orphans.
    pub fn discard_library(&self, library_id: &str) -> Result<(), AppError> {
        validate_library_id(library_id)?;
        let mut connection = self.db()?;
        let root: Option<String> = connection
            .query_row(
                "SELECT root FROM libraries WHERE id = ?1",
                params![library_id],
                |row| row.get(0),
            )
            .optional()?;
        let Some(root) = root else {
            return Err(AppError::LibraryNotFound(library_id.into()));
        };
        let root = PathBuf::from(root);
        if root.exists() {
            fs::remove_dir_all(root)?;
        }
        let transaction = connection.transaction()?;
        transaction.execute(
            "DELETE FROM jobs WHERE library_id = ?1",
            params![library_id],
        )?;
        transaction.execute(
            "DELETE FROM query_runs WHERE library_id = ?1",
            params![library_id],
        )?;
        transaction.execute("DELETE FROM libraries WHERE id = ?1", params![library_id])?;
        transaction.commit()?;
        Ok(())
    }

    /// Replaces one idle library's content tree with a fully prepared
    /// temporary library while preserving the target's id and name. The
    /// replacement must already have passed snapshot validation and repair.
    pub fn replace_library_contents(
        &self,
        target_id: &str,
        replacement_id: &str,
    ) -> Result<Library, AppError> {
        validate_library_id(target_id)?;
        validate_library_id(replacement_id)?;
        if target_id == replacement_id {
            return Err(AppError::BadRequest(
                "replacement library must differ from the target".into(),
            ));
        }

        let mut connection = self.db()?;
        let target = connection
            .query_row(
                "SELECT id, name, description, root, created_at FROM libraries WHERE id = ?1",
                params![target_id],
                library_from_row,
            )
            .optional()?
            .ok_or_else(|| AppError::LibraryNotFound(target_id.into()))?;
        let replacement = connection
            .query_row(
                "SELECT id, name, description, root, created_at FROM libraries WHERE id = ?1",
                params![replacement_id],
                library_from_row,
            )
            .optional()?
            .ok_or_else(|| AppError::LibraryNotFound(replacement_id.into()))?;

        let has_active_job = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM jobs WHERE library_id = ?1 AND status IN (?2, ?3))",
            params![target_id, JobState::Queued, JobState::Running],
            |row| row.get::<_, bool>(0),
        )?;
        let has_active_query = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM query_runs WHERE library_id = ?1 AND status = ?2)",
            params![target_id, JobState::Running],
            |row| row.get::<_, bool>(0),
        )?;
        if has_active_job || has_active_query {
            return Err(AppError::Conflict(format!(
                "library {target_id} has an active job or query"
            )));
        }

        let target_root =
            canonicalize_library_path(&self.root.join("libraries").join(&target.id), &target.id)?;
        if target_root != canonicalize_library_path(Path::new(&target.root), &target.id)? {
            return Err(AppError::Storage(format!(
                "library root is outside its registered content-library path: {target_id}"
            )));
        }
        let replacement_root = canonicalize_library_path(
            &self.root.join("libraries").join(&replacement.id),
            &replacement.id,
        )?;
        if replacement_root
            != canonicalize_library_path(Path::new(&replacement.root), &replacement.id)?
        {
            return Err(AppError::Storage(format!(
                "library root is outside its registered content-library path: {replacement_id}"
            )));
        }

        let backup = self
            .root
            .join("jobs")
            .join(format!("replace-backup-{}", Uuid::new_v4().simple()));
        fs::rename(&target_root, &backup)?;
        if let Err(error) = fs::rename(&replacement_root, &target_root) {
            let restore = fs::rename(&backup, &target_root);
            return match restore {
                Ok(()) => Err(error.into()),
                Err(restore_error) => Err(AppError::Storage(format!(
                    "replacement failed: {error}; restoring original library failed: {restore_error}"
                ))),
            };
        }

        let description = replacement.description.clone();
        let cleanup = (|| -> Result<(), AppError> {
            let transaction = connection.transaction()?;
            transaction.execute("DELETE FROM jobs WHERE library_id = ?1", params![target_id])?;
            transaction.execute(
                "DELETE FROM query_runs WHERE library_id = ?1",
                params![target_id],
            )?;
            transaction.execute(
                "DELETE FROM jobs WHERE library_id = ?1",
                params![replacement_id],
            )?;
            transaction.execute(
                "DELETE FROM query_runs WHERE library_id = ?1",
                params![replacement_id],
            )?;
            transaction.execute(
                "DELETE FROM libraries WHERE id = ?1",
                params![replacement_id],
            )?;
            transaction.execute(
                "UPDATE libraries SET description = ?1 WHERE id = ?2",
                params![description, target_id],
            )?;
            transaction.commit()?;
            Ok(())
        })();
        if let Err(error) = cleanup {
            let restore_replacement = fs::rename(&target_root, &replacement_root);
            let restore_target = fs::rename(&backup, &target_root);
            return match (restore_replacement, restore_target) {
                (Ok(()), Ok(())) => Err(error),
                (replacement_error, target_error) => Err(AppError::Storage(format!(
                    "replacement database update failed: {error}; rollback failed: replacement={replacement_error:?}, target={target_error:?}"
                ))),
            };
        }
        let _ = fs::remove_dir_all(backup);

        Ok(Library {
            description,
            ..target
        })
    }
}

fn library_name_conflict(name: &str) -> AppError {
    AppError::Conflict(format!("a library named {name} already exists"))
}

fn library_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Library> {
    let created_at: String = row.get(4)?;
    Ok(Library {
        id: row.get(0)?,
        name: row.get(1)?,
        description: row.get(2)?,
        root: row.get(3)?,
        created_at: parse_timestamp_or_now(&created_at),
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
        // A task that died waiting for the library lock stays `queued`.
        let queued = storage.create_job(&library.id, JobKind::Ingest).unwrap();
        let query_id = storage.record_query(&library.id).unwrap();

        storage.reap_interrupted_runs().unwrap();

        let interrupted = storage.get_job(&library.id, &interrupted.job_id).unwrap();
        assert_eq!(interrupted.status, JobState::Failed);
        assert_eq!(
            interrupted.error.as_deref(),
            Some("interrupted by server restart")
        );
        let queued = storage.get_job(&library.id, &queued.job_id).unwrap();
        assert_eq!(queued.status, JobState::Failed);
        assert_eq!(
            queued.error.as_deref(),
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
