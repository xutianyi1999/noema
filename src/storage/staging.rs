//! The ingestion staging lifecycle: each ingest job runs the agent in
//! `staging/{job_id}` — a copy of the library's knowledge inputs — and the
//! server promotes only validated knowledge artifacts back into the
//! library root.

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use sha2::{Digest, Sha256};
use walkdir::WalkDir;

use super::{Storage, fsutil::copy_path, layout::LibraryLayout};
use crate::{error::AppError, models::JobState};

impl Storage {
    /// Copy the library's knowledge inputs into `staging/{job_id}` and
    /// capture the protected-path baseline validation compares against.
    /// The baseline comes from the staging copy itself, not the live root:
    /// submissions keep landing in the live `raw/` while a job runs
    /// (document upload is not gated by the ingest lock), and those must
    /// read as baseline matches rather than tampering.
    pub fn prepare_staging(
        &self,
        library_id: &str,
        job_id: &str,
    ) -> Result<(PathBuf, StagingBaseline), AppError> {
        let root = self.library_root(library_id)?;
        let staging = root.join("staging").join(job_id);
        if staging.exists() {
            fs::remove_dir_all(&staging)?;
        }
        fs::create_dir_all(&staging)?;
        for entry in LibraryLayout::STAGING_INPUTS {
            let source = root.join(entry);
            let destination = staging.join(entry);
            if source.exists() {
                copy_path(&source, &destination)?;
            }
        }
        let baseline = StagingBaseline::capture(&staging)?;
        Ok((staging, baseline))
    }

    /// Promote validated staging artifacts into the live library. Every
    /// swap is rename-based (staging lives on the same filesystem as the
    /// root), so each directory is replaced atomically: a failure can never
    /// leave the live tree deleted-but-half-repopulated, and concurrent
    /// readers observe either the old or the new version, never a torn one.
    pub fn promote_staging(&self, library_id: &str, job_id: &str) -> Result<(), AppError> {
        let root = self.library_root(library_id)?;
        let staging = root.join("staging").join(job_id);
        if !staging.exists() {
            return Err(AppError::Storage(format!(
                "missing staging directory {job_id}"
            )));
        }
        for entry in LibraryLayout::PROMOTED_DIRS {
            let source = staging.join(entry);
            if !source.exists() {
                continue;
            }
            let destination = root.join(entry);
            // Park the replaced tree inside this job's staging workspace:
            // it is invisible to exports and index rebuilds (which never
            // walk staging) and is removed with the workspace on cleanup.
            let replaced = staging.join(format!(".replaced-{entry}"));
            if destination.is_dir() {
                fs::rename(&destination, &replaced)?;
            } else if destination.exists() {
                fs::remove_file(&destination)?;
            }
            if let Err(error) = fs::rename(&source, &destination) {
                // Restore the live tree before reporting the failure.
                if replaced.exists() {
                    let _ = fs::rename(&replaced, &destination);
                }
                return Err(error.into());
            }
            if replaced.exists()
                && let Err(error) = fs::remove_dir_all(&replaced)
            {
                tracing::warn!(library_id = %library_id, job_id = %job_id, entry = %entry, %error, "failed to remove replaced tree");
            }
        }
        for entry in LibraryLayout::PROMOTED_FILES {
            let source = staging.join(entry);
            if source.exists() {
                // rename(2) atomically replaces an existing file target.
                fs::rename(&source, root.join(entry))?;
            }
        }
        Ok(())
    }

    pub fn validate_staging(
        &self,
        library_id: &str,
        job_id: &str,
        baseline: &StagingBaseline,
    ) -> Result<(), AppError> {
        self.validate_staging_inner(library_id, job_id, baseline, true)
    }

    /// Like [`validate_staging`] but permits an empty `wiki/`: deleting a
    /// library's last source document legitimately leaves no knowledge nodes
    /// to promote.
    pub fn validate_staging_after_delete(
        &self,
        library_id: &str,
        job_id: &str,
        baseline: &StagingBaseline,
    ) -> Result<(), AppError> {
        self.validate_staging_inner(library_id, job_id, baseline, false)
    }

    fn validate_staging_inner(
        &self,
        library_id: &str,
        job_id: &str,
        baseline: &StagingBaseline,
        require_wiki_nodes: bool,
    ) -> Result<(), AppError> {
        let staging = self.library_root(library_id)?.join("staging").join(job_id);
        if !staging.is_dir() {
            return Err(AppError::Storage(format!(
                "missing staging directory {job_id}"
            )));
        }

        for entry in WalkDir::new(&staging).follow_links(false) {
            let entry = entry?;
            if entry.file_type().is_symlink() {
                let relative = entry.path().strip_prefix(&staging)?;
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

        for entry in fs::read_dir(&staging)? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !LibraryLayout::STAGING_INPUTS.contains(&name.as_ref()) {
                return Err(AppError::Storage(format!(
                    "staging contains an unauthorized top-level path: {name}"
                )));
            }
        }

        // The Agent is allowed to install/update runtime dependencies in the
        // staging project's .opencode directory. It is intentionally not part
        // of the promoted output, so only knowledge inputs and the library
        // contract are compared against the baseline captured at preparation
        // time — not the live root, which concurrent submissions keep
        // changing while this job runs.
        for relative in LibraryLayout::PROTECTED_PATHS {
            let expected = baseline
                .digests
                .get(relative)
                .and_then(|digest| digest.as_deref());
            if tree_digest(&staging.join(relative))?.as_deref() != expected {
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

        // Shape of the promoted outputs: every ingest compiles knowledge
        // nodes, and each output path must be the type promotion swaps in.
        // Without this an agent that deleted `wiki/` (or replaced it with a
        // regular file) would pass validation and either report a job
        // completed that compiled nothing or promote a file over the live
        // wiki tree. Maintenance after a delete may legitimately empty the
        // wiki (the caller relaxes `require_wiki_nodes`), but `wiki/` must
        // still be a directory.
        let wiki = staging.join("wiki");
        if !wiki.is_dir() {
            return Err(AppError::Storage("staging has no wiki/ directory".into()));
        }
        if require_wiki_nodes && !contains_markdown(&wiki)? {
            return Err(AppError::Storage(
                "staging has no wiki/ nodes: the ingest compiled no knowledge".into(),
            ));
        }
        for entry in LibraryLayout::PROMOTED_DIRS {
            let path = staging.join(entry);
            if path.exists() && !path.is_dir() {
                return Err(AppError::Storage(format!(
                    "staging output must be a directory: {entry}"
                )));
            }
        }
        for entry in LibraryLayout::PROMOTED_FILES {
            let path = staging.join(entry);
            if path.exists() && !path.is_file() {
                return Err(AppError::Storage(format!(
                    "staging output must be a file: {entry}"
                )));
            }
        }

        // Node content is the agent's responsibility: noema checks only the
        // shape of the promoted outputs above, never the knowledge itself.
        Ok(())
    }

    pub fn cleanup_staging(&self, library_id: &str, job_id: &str) -> Result<(), AppError> {
        let staging = self.library_root(library_id)?.join("staging").join(job_id);
        if staging.exists() {
            fs::remove_dir_all(staging)?;
        }
        Ok(())
    }

    /// Remove ingest workspaces that are no longer needed: those of
    /// successfully finished jobs (completed or skipped) and orphans whose
    /// job row is gone. Failed workspaces stay for inspection; in-flight
    /// ones are left alone.
    ///
    /// OpenCode persists session state under the session's project
    /// directory asynchronously, so it can resurrect a cleaned staging
    /// directory later as an empty skeleton of session tombstones. Running
    /// this at startup and again after each completed ingest makes staging
    /// hygiene a convergent invariant instead of a race with that
    /// write-back.
    pub fn reconcile_staging(&self, library_id: &str) -> Result<(), AppError> {
        let staging = self.library_root(library_id)?.join("staging");
        if !staging.is_dir() {
            return Ok(());
        }
        for entry in fs::read_dir(&staging)? {
            let entry = entry?;
            let job_id = entry.file_name().to_string_lossy().into_owned();
            let expendable = match self.get_job(library_id, &job_id) {
                Ok(job) => matches!(job.status, JobState::Completed | JobState::Skipped),
                Err(AppError::JobNotFound(_)) => true,
                Err(error) => return Err(error),
            };
            if expendable {
                fs::remove_dir_all(entry.path())?;
            }
        }
        Ok(())
    }
}

/// Per-protected-path content digests of a freshly prepared staging copy.
/// Validation recomputes the same digests after the session ran: staging is
/// sealed while a job runs (the ingest lock serializes jobs per library and
/// nothing else writes there), so any difference was written by the session
/// itself.
#[derive(Debug, Clone)]
pub struct StagingBaseline {
    digests: BTreeMap<String, Option<String>>,
}

impl StagingBaseline {
    fn capture(staging: &Path) -> Result<Self, AppError> {
        let mut digests = BTreeMap::new();
        for relative in LibraryLayout::PROTECTED_PATHS {
            digests.insert(relative.to_string(), tree_digest(&staging.join(relative))?);
        }
        Ok(Self { digests })
    }
}

/// Content digest of one staging path — file or subtree — as a sha256 over
/// every file's relative path and content, sorted so directory iteration
/// order cannot matter. `None` when the path is absent.
fn tree_digest(path: &Path) -> Result<Option<String>, AppError> {
    if !path.exists() {
        return Ok(None);
    }
    let mut files = Vec::new();
    for entry in WalkDir::new(path).follow_links(false) {
        let entry = entry?;
        if entry.file_type().is_file() {
            files.push(entry.path().to_path_buf());
        }
    }
    files.sort();
    let mut hasher = Sha256::new();
    for file in files {
        let relative = file.strip_prefix(path)?;
        hasher.update(relative.to_string_lossy().as_bytes());
        hasher.update(b"\0");
        hasher.update(fs::read(file)?);
        hasher.update(b"\n");
    }
    Ok(Some(hex::encode(hasher.finalize())))
}

/// Whether the tree contains at least one markdown file.
fn contains_markdown(root: &Path) -> Result<bool, AppError> {
    for entry in WalkDir::new(root).follow_links(false) {
        let entry = entry?;
        let is_markdown = entry.file_type().is_file()
            && entry
                .path()
                .extension()
                .and_then(|value| value.to_str())
                .is_some_and(|value| value.eq_ignore_ascii_case("md"));
        if is_markdown {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{CreateLibraryRequest, JobKind};

    fn fixture() -> (tempfile::TempDir, Storage, String) {
        let tmp = tempfile::tempdir().unwrap();
        let storage = Storage::open(tmp.path()).unwrap();
        let library = storage
            .create_library(&CreateLibraryRequest {
                name: "法规库".into(),
                description: None,
            })
            .unwrap();
        (tmp, storage, library.id)
    }

    #[test]
    fn reconcile_staging_deletes_terminal_and_orphan_workspaces_but_keeps_the_rest() {
        let (tmp, storage, library_id) = fixture();
        let staging = tmp
            .path()
            .join("libraries")
            .join(&library_id)
            .join("staging");

        let completed = storage.create_job(&library_id, JobKind::Ingest).unwrap();
        storage
            .update_job(
                &library_id,
                &completed.job_id,
                JobState::Completed,
                None,
                None,
            )
            .unwrap();
        let failed = storage.create_job(&library_id, JobKind::Ingest).unwrap();
        storage
            .update_job(
                &library_id,
                &failed.job_id,
                JobState::Failed,
                None,
                Some("boom"),
            )
            .unwrap();
        let running = storage.create_job(&library_id, JobKind::Ingest).unwrap();
        storage
            .update_job(&library_id, &running.job_id, JobState::Running, None, None)
            .unwrap();
        for job in [&completed, &failed, &running] {
            fs::create_dir_all(staging.join(&job.job_id)).unwrap();
        }
        fs::create_dir_all(staging.join("orphan-without-a-job-row")).unwrap();

        storage.reconcile_staging(&library_id).unwrap();

        assert!(
            !staging.join(&completed.job_id).exists(),
            "completed workspaces are expendable"
        );
        assert!(
            !staging.join("orphan-without-a-job-row").exists(),
            "orphan workspaces are expendable"
        );
        assert!(
            staging.join(&failed.job_id).exists(),
            "failed workspaces are kept for inspection"
        );
        assert!(
            staging.join(&running.job_id).exists(),
            "in-flight workspaces are left alone"
        );
    }

    #[test]
    fn reconcile_staging_tolerates_a_library_without_a_staging_directory() {
        let (_tmp, storage, library_id) = fixture();
        storage.reconcile_staging(&library_id).unwrap();
    }

    #[test]
    fn validate_staging_compares_against_the_preparation_baseline_not_the_live_root() {
        let (_tmp, storage, library_id) = fixture();
        storage
            .store_document(&library_id, "source.md", None, "# Source\n\nBody.", None)
            .unwrap();
        let job = storage.create_job(&library_id, JobKind::Ingest).unwrap();
        let (staging, baseline) = storage.prepare_staging(&library_id, &job.job_id).unwrap();

        // A concurrent submission lands in the live raw/ (and a manual edit
        // rewrites purpose.md) while the job runs: validation compares the
        // staging copy against its preparation baseline and ignores the live
        // root entirely.
        let root = storage.library_root(&library_id).unwrap();
        fs::write(root.join("raw/late.md"), "# Late arrival").unwrap();
        fs::write(root.join("purpose.md"), "rewritten live").unwrap();
        // A successful ingest compiled at least one node.
        fs::write(staging.join("wiki/n.md"), contract_node()).unwrap();
        storage
            .validate_staging(&library_id, &job.job_id, &baseline)
            .unwrap();
    }

    #[test]
    fn validate_staging_accepts_nodes_of_any_shape() {
        // Knowledge structure is the agent's responsibility: noema checks
        // only that the wiki has content, never how a node is written. A
        // node with no frontmatter and a free-form body is a valid output.
        let (_tmp, storage, library_id) = fixture();
        storage
            .store_document(&library_id, "source.md", None, "# Source\n\nBody.", None)
            .unwrap();
        let job = storage.create_job(&library_id, JobKind::Ingest).unwrap();
        let (staging, baseline) = storage.prepare_staging(&library_id, &job.job_id).unwrap();
        fs::write(
            staging.join("wiki/freeform.md"),
            "# 自由节点\n\n任意正文，无 frontmatter。\n",
        )
        .unwrap();
        storage
            .validate_staging(&library_id, &job.job_id, &baseline)
            .unwrap();
    }

    /// A knowledge node: plain markdown. Noema validates only that `wiki/`
    /// has content — a node's structure is the agent's responsibility.
    fn contract_node() -> String {
        "# N\n\nA test knowledge node.\n".to_string()
    }

    #[test]
    fn validate_staging_rejects_every_modification_inside_staging() {
        let (_tmp, storage, library_id) = fixture();
        storage
            .store_document(&library_id, "source.md", None, "# Source\n\nBody.", None)
            .unwrap();
        let job = storage.create_job(&library_id, JobKind::Ingest).unwrap();
        let (staging, baseline) = storage.prepare_staging(&library_id, &job.job_id).unwrap();
        let purpose = fs::read_to_string(staging.join("purpose.md")).unwrap();

        // A file added under a protected staging path.
        fs::write(staging.join("raw/smuggled.md"), "# smuggled").unwrap();
        let error = storage
            .validate_staging(&library_id, &job.job_id, &baseline)
            .unwrap_err();
        assert!(error.to_string().contains("protected path: raw"), "{error}");
        fs::remove_file(staging.join("raw/smuggled.md")).unwrap();

        // A protected staging file edited.
        fs::write(staging.join("purpose.md"), "rewritten by the agent").unwrap();
        let error = storage
            .validate_staging(&library_id, &job.job_id, &baseline)
            .unwrap_err();
        assert!(
            error.to_string().contains("protected path: purpose.md"),
            "{error}"
        );
        fs::write(staging.join("purpose.md"), purpose).unwrap();

        // A protected file deleted from staging.
        fs::remove_file(staging.join("raw/source.md")).unwrap();
        let error = storage
            .validate_staging(&library_id, &job.job_id, &baseline)
            .unwrap_err();
        assert!(error.to_string().contains("protected path: raw"), "{error}");
        fs::write(staging.join("raw/source.md"), "# Source\n\nBody.").unwrap();

        // Restored byte-for-byte: the digests match again once the ingest's
        // compiled node is in place.
        fs::write(staging.join("wiki/n.md"), contract_node()).unwrap();
        storage
            .validate_staging(&library_id, &job.job_id, &baseline)
            .unwrap();

        // A whole-directory deletion of a promoted output is rejected —
        // promotion would silently keep the old tree and report success.
        fs::remove_dir_all(staging.join("wiki")).unwrap();
        let error = storage
            .validate_staging(&library_id, &job.job_id, &baseline)
            .unwrap_err();
        assert!(error.to_string().contains("wiki"), "{error}");
        fs::create_dir_all(staging.join("wiki")).unwrap();
        fs::write(staging.join("wiki/n.md"), contract_node()).unwrap();

        // Replacing a promoted directory with a regular file as well.
        fs::remove_dir_all(staging.join("wiki")).unwrap();
        fs::write(staging.join("wiki"), "not a directory").unwrap();
        let error = storage
            .validate_staging(&library_id, &job.job_id, &baseline)
            .unwrap_err();
        assert!(error.to_string().contains("wiki"), "{error}");
    }
}
