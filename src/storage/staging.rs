//! The ingestion staging lifecycle: each ingest job runs the agent in
//! `staging/{job_id}` — a copy of the library's knowledge inputs — and the
//! server promotes only validated knowledge artifacts back into the
//! library root.

use std::{
    fs,
    path::{Path, PathBuf},
};

use walkdir::WalkDir;

use super::{Storage, fsutil::copy_path, layout::LibraryLayout};
use crate::{error::AppError, models::JobState};

impl Storage {
    pub fn prepare_staging(&self, library_id: &str, job_id: &str) -> Result<PathBuf, AppError> {
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
        for entry in LibraryLayout::PROMOTED_DIRS {
            let source = staging.join(entry);
            let destination = root.join(entry);
            if source.exists() {
                if destination.exists() {
                    fs::remove_dir_all(&destination)?;
                }
                copy_path(&source, &destination)?;
            }
        }
        for entry in LibraryLayout::PROMOTED_FILES {
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
        // contract are compared against the baseline.
        for relative in LibraryLayout::PROTECTED_PATHS {
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
        let entry = entry?;
        let relative = entry.path().strip_prefix(root)?.to_path_buf();
        entries.push((relative, entry.file_type().is_dir()));
    }
    entries.sort();
    Ok(entries)
}

/// The frontmatter keys every wiki node must declare — the node contract,
/// documented in schema.md and enforced by the knowledge-compiler skill.
const WIKI_NODE_KEYS: [&str; 9] = [
    "node_id",
    "canonical_name",
    "kind",
    "sources",
    "relations",
    "claim_type",
    "confidence",
    "created_at",
    "updated_at",
];

/// The YAML frontmatter block of a node, if the file opens with a `---`
/// line closed by a second `---` line.
fn extract_frontmatter(content: &str) -> Option<String> {
    let mut lines = content.lines();
    if lines.next() != Some("---") {
        return None;
    }
    let mut frontmatter = String::new();
    for line in lines {
        if line == "---" {
            return Some(frontmatter);
        }
        frontmatter.push_str(line);
        frontmatter.push('\n');
    }
    None
}

fn validate_wiki_nodes(root: &Path) -> Result<(), AppError> {
    if !root.exists() {
        return Ok(());
    }
    for entry in WalkDir::new(root).follow_links(false) {
        let entry = entry?;
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
        let Some(frontmatter) = extract_frontmatter(&content) else {
            return Err(AppError::Storage(format!(
                "wiki node is missing YAML frontmatter: {}",
                entry.path().display()
            )));
        };
        let mapping: yaml_serde::Mapping = yaml_serde::from_str(&frontmatter).map_err(|error| {
            AppError::Storage(format!(
                "wiki node has invalid YAML frontmatter: {}: {error}",
                entry.path().display()
            ))
        })?;
        for key in WIKI_NODE_KEYS {
            if !mapping.contains_key(yaml_serde::Value::String(key.into())) {
                return Err(AppError::Storage(format!(
                    "wiki node has incomplete YAML frontmatter: {}: missing `{key}`",
                    entry.path().display()
                )));
            }
        }
    }
    Ok(())
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
}
