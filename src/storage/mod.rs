//! Content-library persistence: the control-plane database (libraries,
//! jobs, query runs) plus each library's on-disk tree and per-library
//! database (documents, full-text index).
//!
//! The `Storage` impl is split across submodules by concern: `control`
//! (control-plane CRUD), `documents` (per-library document store, derived
//! index and FTS), `staging` (the ingest workspace lifecycle) and `layout`
//! (the on-disk shape of a library); `fsutil` holds the shared file
//! helpers.

use std::{
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};

use chrono::{DateTime, Utc};
use rusqlite::Connection;
use serde::Serialize;

mod control;
mod documents;
mod fsutil;
mod layout;
mod staging;

pub(crate) use documents::knowledge_files;
pub(crate) use fsutil::copy_path;

use crate::error::AppError;

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
    /// Opaque caller-defined JSON set at submission time; `None` for rows
    /// stored before the metadata column existed (or submitted without it).
    #[serde(default)]
    pub metadata: Option<serde_json::Value>,
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
        connection.busy_timeout(Duration::from_secs(5))?;
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
            CREATE INDEX IF NOT EXISTS query_runs_library_session_idx
                ON query_runs (library_id, session_id);
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

    /// Roll back a library whose bootstrap or snapshot import failed:
    /// attempt to discard the row and directory tree, logging any cleanup
    /// failure without masking the caller's original error.
    pub(crate) fn discard_on_failure(&self, library_id: &str, context: &str) {
        if let Err(error) = self.discard_library(library_id) {
            tracing::error!(library_id = %library_id, %error, "failed to roll back {context}");
        }
    }
}

/// Parses an RFC-3339 timestamp read from the database. Corrupt values
/// degrade to `now` rather than failing the whole row: timestamps are
/// display metadata, not identity.
fn parse_timestamp_or_now(value: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(value)
        .map(|value| value.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now())
}

/// Open a library-local database connection with the busy timeout shared by
/// every connection to the same file. Per-library ingest serialization
/// removes most contention, but an upload can still race an index rebuild;
/// a short retry window turns that collision into a wait instead of a
/// `database is locked` failure.
fn open_library_db(path: &Path) -> Result<Connection, AppError> {
    let connection = Connection::open(path)?;
    connection.busy_timeout(Duration::from_secs(5))?;
    Ok(connection)
}
