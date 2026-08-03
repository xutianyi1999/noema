//! Per-library document store: the documents table and `raw/` files of one
//! content library, plus the derived `index.md`, `manifest.json` and the
//! content full-text index.

use std::{
    fs,
    path::{Path, PathBuf},
};

use chrono::Utc;
use rusqlite::{OptionalExtension, params};
use sha2::{Digest, Sha256};
use unicode_normalization::UnicodeNormalization;
use uuid::Uuid;
use walkdir::WalkDir;

use super::{
    DocumentRecord, Storage, StoredDocument, fsutil::write_atomic, open_library_db,
    parse_timestamp_or_now,
};
use crate::error::AppError;

impl Storage {
    pub fn store_document(
        &self,
        library_id: &str,
        filename: &str,
        title: Option<&str>,
        content: &str,
    ) -> Result<StoredDocument, AppError> {
        let root = self.library_root(library_id)?;
        // Filenames are kept verbatim (NFC-normalized so visually identical
        // names share one spelling), so they must stay single-component and
        // citation-safe.
        let filename: String = filename.nfc().collect();
        validate_filename(&filename)?;
        let mut hasher = Sha256::new();
        hasher.update(content.as_bytes());
        let sha256 = hex::encode(hasher.finalize());
        let now = Utc::now();
        let connection = open_library_db(&root.join("library.sqlite"))?;

        if let Some(existing) = find_by_sha256(&connection, &sha256)? {
            return Ok(StoredDocument {
                record: existing,
                duplicate: true,
            });
        }

        // The content is new; a document with the same name is therefore a
        // different file — raw/ names are stable identities, not a namespace
        // for versions, so the uploader must rename rather than clobber.
        let name_taken = connection
            .query_row(
                "SELECT 1 FROM documents WHERE filename = ?1",
                params![filename],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if name_taken {
            return Err(name_conflict(&filename));
        }

        let id = Uuid::new_v4().to_string();
        let relative_path = format!("raw/{filename}");
        let title = title
            .filter(|value| !value.trim().is_empty())
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| {
                Path::new(&filename)
                    .file_stem()
                    .and_then(|value| value.to_str())
                    .unwrap_or(&filename)
                    .to_string()
            });
        let record = DocumentRecord {
            id,
            filename,
            title,
            path: relative_path,
            sha256,
            created_at: now,
        };
        // Register the document BEFORE writing the raw/ file: the INSERT's
        // UNIQUE constraints (sha256, path) arbitrate concurrent uploads, so
        // whoever inserts first owns the filename and the loser can never
        // overwrite the winner's file on disk (which would leave the DB
        // checksum describing different content than raw/ holds).
        match connection.execute(
            "INSERT INTO documents (id, filename, title, path, sha256, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                record.id,
                record.filename,
                record.title,
                record.path,
                record.sha256,
                record.created_at.to_rfc3339(),
            ],
        ) {
            Ok(_) => {}
            // Lost a race the pre-checks above could not see: identical
            // content landed concurrently → duplicate; the filename was
            // taken with different content → conflict.
            Err(rusqlite::Error::SqliteFailure(failure, _))
                if failure.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                if let Some(existing) = find_by_sha256(&connection, &record.sha256)? {
                    return Ok(StoredDocument {
                        record: existing,
                        duplicate: true,
                    });
                }
                return Err(name_conflict(&record.filename));
            }
            Err(error) => return Err(error.into()),
        }
        if let Err(error) = write_atomic(&root.join(&record.path), content.as_bytes()) {
            // Keep raw/ and the documents table consistent: no file, no
            // record.
            let _ = connection.execute("DELETE FROM documents WHERE id = ?1", params![record.id]);
            return Err(error);
        }
        // The document is durably stored now; a manifest refresh failure
        // must not fail the submission. The manifest is derived data — the
        // next index rebuild regenerates it.
        if let Err(error) = self.write_manifest(library_id) {
            tracing::warn!(library_id = %library_id, %error, "manifest refresh failed");
        }
        Ok(StoredDocument {
            record,
            duplicate: false,
        })
    }

    /// Drop document rows whose `raw/` file is missing — the residue of a
    /// crash between the row's INSERT and the file write. Such a row blocks
    /// the document forever: dedupe treats the content as already stored,
    /// yet ingest would be told to read a file that does not exist. Runs at
    /// startup, when no ingest is in flight; the document becomes
    /// submittable again.
    pub fn reconcile_documents(&self, library_id: &str) -> Result<(), AppError> {
        let root = self.library_root(library_id)?;
        let connection = open_library_db(&root.join("library.sqlite"))?;
        let rows = {
            let mut statement = connection.prepare("SELECT id, path FROM documents")?;
            statement
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<Result<Vec<_>, _>>()?
        };
        for (id, path) in rows {
            if !root.join(&path).is_file() {
                tracing::warn!(library_id = %library_id, %path, "removing document row whose raw/ file is missing");
                connection.execute("DELETE FROM documents WHERE id = ?1", params![id])?;
            }
        }
        Ok(())
    }

    /// Every document record of one library in submission order (oldest
    /// first) — the raw/ side of what ingestion must have compiled.
    pub fn list_documents(&self, library_id: &str) -> Result<Vec<DocumentRecord>, AppError> {
        let root = self.library_root(library_id)?;
        let connection = open_library_db(&root.join("library.sqlite"))?;
        let mut statement = connection.prepare(
            "SELECT id, filename, title, path, sha256, created_at FROM documents ORDER BY created_at",
        )?;
        Ok(statement
            .query_map([], document_from_row)?
            .collect::<Result<Vec<_>, _>>()?)
    }

    pub fn write_manifest(&self, library_id: &str) -> Result<(), AppError> {
        let root = self.library_root(library_id)?;
        let documents = self.list_documents(library_id)?;
        let manifest = serde_json::json!({ "documents": documents });
        write_atomic(
            &root.join("manifest.json"),
            serde_json::to_vec_pretty(&manifest)?.as_slice(),
        )?;
        Ok(())
    }

    pub fn rebuild_index(&self, library_id: &str) -> Result<(), AppError> {
        let root = self.library_root(library_id)?;
        let files = knowledge_files(&root)?;
        let mut output = String::from("# Knowledge Library Index\n\n");
        output.push_str("This file is generated by Noema and is a navigation aid.\n\n");
        for (label, prefix) in [("Sources", "raw/"), ("Knowledge nodes", "wiki/")] {
            output.push_str(&format!("## {label}\n\n"));
            for (relative, _) in files
                .iter()
                .filter(|(relative, _)| relative.starts_with(prefix))
            {
                output.push_str(&format!("- `{relative}`\n"));
            }
            output.push('\n');
        }
        write_atomic(&root.join("index.md"), output.as_bytes())?;
        rebuild_content_fts(&root)?;
        self.write_manifest(library_id)
    }
}

pub(super) fn init_library_db(path: &Path) -> Result<(), AppError> {
    let connection = open_library_db(path)?;
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
        CREATE VIRTUAL TABLE IF NOT EXISTS content_fts USING fts5(path, title, body);
        ",
    )?;
    Ok(())
}

/// Rebuild batch size: each batch is its own write transaction, so a
/// rebuild of a large library releases the writer lock between batches and
/// concurrent uploads wait at most one batch instead of the whole rebuild
/// (the previous single transaction exceeded the busy timeout and failed
/// them outright).
const FTS_REBUILD_BATCH: usize = 256;

fn rebuild_content_fts(root: &Path) -> Result<(), AppError> {
    let mut connection = open_library_db(&root.join("library.sqlite"))?;
    // Build into a shadow table, then swap it in with one short
    // transaction: readers never observe an empty or half-built index,
    // writers are barely held. A leftover shadow from a crashed rebuild is
    // dropped first.
    connection.execute_batch(
        "DROP TABLE IF EXISTS content_fts_rebuild;
         CREATE VIRTUAL TABLE content_fts_rebuild USING fts5(path, title, body);",
    )?;
    let files = knowledge_files(root)?;
    let result = rebuild_fts_batches(&mut connection, &files);
    if result.is_err() {
        let _ = connection.execute_batch("DROP TABLE IF EXISTS content_fts_rebuild");
        return result;
    }
    let transaction = connection.transaction()?;
    transaction.execute_batch(
        "DROP TABLE IF EXISTS content_fts;
         ALTER TABLE content_fts_rebuild RENAME TO content_fts;",
    )?;
    transaction.commit()?;
    Ok(())
}

fn rebuild_fts_batches(
    connection: &mut rusqlite::Connection,
    files: &[(String, PathBuf)],
) -> Result<(), AppError> {
    for batch in files.chunks(FTS_REBUILD_BATCH) {
        let transaction = connection.transaction()?;
        for (relative, path) in batch {
            let body = fs::read_to_string(path).map_err(|error| {
                AppError::Storage(format!(
                    "knowledge file is not valid UTF-8 text: {}: {error}",
                    path.display()
                ))
            })?;
            let title = path
                .file_stem()
                .and_then(|value| value.to_str())
                .unwrap_or_default();
            transaction.execute(
                "INSERT INTO content_fts_rebuild (path, title, body) VALUES (?1, ?2, ?3)",
                params![relative, title, body],
            )?;
        }
        transaction.commit()?;
    }
    Ok(())
}

/// Every file the derived artifacts (index.md, the content FTS) cover:
/// `raw/` as .md/.txt and `wiki/` as .md, matched case-insensitively
/// (uploads keep their extension casing verbatim), sorted by relative path.
pub(crate) fn knowledge_files(root: &Path) -> Result<Vec<(String, PathBuf)>, AppError> {
    let mut files: Vec<(String, PathBuf)> = Vec::new();
    for (directory, extensions) in [
        ("raw", ["md", "txt"].as_slice()),
        ("wiki", ["md"].as_slice()),
    ] {
        let directory_path = root.join(directory);
        if !directory_path.exists() {
            continue;
        }
        for entry in WalkDir::new(&directory_path).follow_links(false) {
            let entry = entry?;
            let path = entry.path();
            let matches = entry.file_type().is_file()
                && path
                    .extension()
                    .and_then(|value| value.to_str())
                    .is_some_and(|extension| {
                        extensions
                            .iter()
                            .any(|allowed| extension.eq_ignore_ascii_case(allowed))
                    });
            if matches {
                let relative = path.strip_prefix(root)?.display().to_string();
                files.push((relative, path.to_path_buf()));
            }
        }
    }
    files.sort();
    Ok(files)
}

fn find_by_sha256(
    connection: &rusqlite::Connection,
    sha256: &str,
) -> Result<Option<DocumentRecord>, AppError> {
    Ok(connection
        .query_row(
            "SELECT id, filename, title, path, sha256, created_at FROM documents WHERE sha256 = ?1",
            params![sha256],
            document_from_row,
        )
        .optional()?)
}

fn name_conflict(filename: &str) -> AppError {
    AppError::Conflict(format!(
        "a document named {filename} already exists with different content"
    ))
}

fn document_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<DocumentRecord> {
    let created_at: String = row.get(5)?;
    Ok(DocumentRecord {
        id: row.get(0)?,
        filename: row.get(1)?,
        title: row.get(2)?,
        path: row.get(3)?,
        sha256: row.get(4)?,
        created_at: parse_timestamp_or_now(&created_at),
    })
}

fn validate_filename(value: &str) -> Result<(), AppError> {
    let path = Path::new(value);
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default();
    if value.is_empty()
        || value != value.trim()
        || value.contains('/')
        || value.contains('\\')
        || value == "."
        || value == ".."
        || value.chars().any(char::is_control)
        || !matches!(extension.to_ascii_lowercase().as_str(), "md" | "txt")
    {
        return Err(AppError::BadRequest(
            "filename must be a single .md or .txt filename".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::CreateLibraryRequest;

    fn fixture() -> (tempfile::TempDir, Storage, String) {
        let tmp = tempfile::tempdir().unwrap();
        let storage = Storage::open(tmp.path()).unwrap();
        let library = storage
            .create_library(&CreateLibraryRequest {
                name: "文档库".into(),
                description: None,
            })
            .unwrap();
        (tmp, storage, library.id)
    }

    #[test]
    fn concurrent_uploads_of_the_same_name_never_corrupt_the_sha256_invariant() {
        let (_tmp, storage, library_id) = fixture();
        let results = std::thread::scope(|scope| {
            (0..2)
                .map(|index| {
                    let storage = storage.clone();
                    let library_id = library_id.clone();
                    scope.spawn(move || {
                        storage.store_document(
                            &library_id,
                            "race.md",
                            None,
                            &format!("content {index}"),
                        )
                    })
                })
                .map(|handle| handle.join().unwrap())
                .collect::<Vec<_>>()
        });
        let successes = results.iter().filter(|result| result.is_ok()).count();
        assert_eq!(successes, 1, "{results:?}");
        assert!(
            results
                .iter()
                .any(|result| matches!(result, Err(AppError::Conflict(_)))),
            "{results:?}"
        );
        // The surviving DB record describes exactly what is on disk.
        let root = storage.library_root(&library_id).unwrap();
        let on_disk = fs::read_to_string(root.join("raw/race.md")).unwrap();
        let connection = rusqlite::Connection::open(root.join("library.sqlite")).unwrap();
        let (path, sha256): (String, String) = connection
            .query_row("SELECT path, sha256 FROM documents", [], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .unwrap();
        assert_eq!(path, "raw/race.md");
        let mut hasher = Sha256::new();
        hasher.update(on_disk.as_bytes());
        assert_eq!(sha256, hex::encode(hasher.finalize()));
    }

    #[test]
    fn reconcile_drops_phantom_rows_and_unblocks_resubmission() {
        let (_tmp, storage, library_id) = fixture();
        storage
            .store_document(&library_id, "phantom.md", None, "content")
            .unwrap();
        // Simulate the crash window's aftermath: the row exists, the file
        // does not.
        let root = storage.library_root(&library_id).unwrap();
        fs::remove_file(root.join("raw/phantom.md")).unwrap();
        storage.reconcile_documents(&library_id).unwrap();
        assert!(storage.list_documents(&library_id).unwrap().is_empty());
        // Without reconciliation dedupe would keep reporting a duplicate of
        // content whose file no longer exists.
        let stored = storage
            .store_document(&library_id, "phantom.md", None, "content")
            .unwrap();
        assert!(!stored.duplicate);
    }

    #[test]
    fn concurrent_uploads_of_the_same_content_report_exactly_one_duplicate() {
        let (_tmp, storage, library_id) = fixture();
        let results = std::thread::scope(|scope| {
            ["a.md", "b.md"]
                .map(|filename| {
                    let storage = storage.clone();
                    let library_id = library_id.clone();
                    scope.spawn(move || {
                        storage.store_document(&library_id, filename, None, "identical")
                    })
                })
                .map(|handle| handle.join().unwrap())
        });
        let fresh = results
            .iter()
            .filter(|result| result.as_ref().is_ok_and(|stored| !stored.duplicate))
            .count();
        let duplicates = results
            .iter()
            .filter(|result| result.as_ref().is_ok_and(|stored| stored.duplicate))
            .count();
        assert_eq!(fresh, 1, "{results:?}");
        assert_eq!(duplicates, 1, "{results:?}");
        // Exactly one raw/ file: the duplicate never wrote a second copy.
        let root = storage.library_root(&library_id).unwrap();
        assert_eq!(fs::read_dir(root.join("raw")).unwrap().count(), 1);
    }
}
