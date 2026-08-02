//! Content-library persistence: the control-plane database (libraries,
//! jobs, query runs) plus each library's on-disk tree and per-library
//! database (documents, node registry, full-text index).
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
};

use chrono::{DateTime, Utc};
use rusqlite::Connection;
use serde::Serialize;

mod control;
mod documents;
mod fsutil;
mod layout;
mod staging;

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
}

/// Parses an RFC-3339 timestamp read from the database. Corrupt values
/// degrade to `now` rather than failing the whole row: timestamps are
/// display metadata, not identity.
fn parse_timestamp_or_now(value: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(value)
        .map(|value| value.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now())
}
