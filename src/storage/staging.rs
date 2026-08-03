//! The ingestion staging lifecycle: each ingest job runs the agent in
//! `staging/{job_id}` — a copy of the library's knowledge inputs — and the
//! server promotes only validated knowledge artifacts back into the
//! library root.

use std::{
    collections::{BTreeMap, HashSet},
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

    pub fn validate_staging(
        &self,
        library_id: &str,
        job_id: &str,
        baseline: &StagingBaseline,
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

/// Every `raw/` path some wiki node claims in its `sources` frontmatter —
/// the set of documents ingestion has already compiled. Used to name the
/// documents a failed job left behind in `raw/` without nodes, and to tell
/// a genuine no-op duplicate from one that must be re-ingested. A missing
/// directory yields the empty set; files without frontmatter or with
/// malformed `sources` entries contribute nothing; unparseable YAML is an
/// error, as in [`validate_wiki_nodes`].
pub(crate) fn referenced_sources(wiki_dir: &Path) -> Result<HashSet<String>, AppError> {
    if !wiki_dir.exists() {
        return Ok(HashSet::new());
    }
    let mut referenced = HashSet::new();
    for entry in WalkDir::new(wiki_dir).follow_links(false) {
        let entry = entry?;
        if !entry.file_type().is_file() {
            continue;
        }
        let Some(mapping) = frontmatter_mapping(entry.path())? else {
            continue;
        };
        let Some(yaml_serde::Value::Sequence(items)) = mapping.get("sources") else {
            continue;
        };
        for item in items {
            // The contract shape is a `{path: ..., locator: ...}` mapping; a
            // bare string is tolerated.
            let path = match item {
                yaml_serde::Value::Mapping(item) => item.get("path"),
                plain => Some(plain),
            };
            if let Some(yaml_serde::Value::String(path)) = path {
                referenced.insert(path.clone());
            }
        }
    }
    Ok(referenced)
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

/// The YAML frontmatter of one node file as a mapping, or `None` when the
/// file has no frontmatter block; unparseable YAML is an error.
fn frontmatter_mapping(path: &Path) -> Result<Option<yaml_serde::Mapping>, AppError> {
    let content = fs::read_to_string(path)?;
    let Some(frontmatter) = extract_frontmatter(&content) else {
        return Ok(None);
    };
    let mapping = yaml_serde::from_str(&frontmatter).map_err(|error| {
        AppError::Storage(format!(
            "wiki node has invalid YAML frontmatter: {}: {error}",
            path.display()
        ))
    })?;
    Ok(Some(mapping))
}

fn validate_wiki_nodes(root: &Path) -> Result<(), AppError> {
    if !root.exists() {
        return Ok(());
    }
    for entry in WalkDir::new(root).follow_links(false) {
        let entry = entry?;
        let path = entry.path();
        if !entry.file_type().is_file()
            || path
                .extension()
                .and_then(|value| value.to_str())
                .is_none_or(|value| !value.eq_ignore_ascii_case("md"))
        {
            continue;
        }
        let Some(mapping) = frontmatter_mapping(path)? else {
            return Err(AppError::Storage(format!(
                "wiki node is missing YAML frontmatter: {}",
                path.display()
            )));
        };
        let missing: Vec<&str> = WIKI_NODE_KEYS
            .iter()
            .copied()
            .filter(|key| !mapping.contains_key(yaml_serde::Value::String((*key).into())))
            .collect();
        if !missing.is_empty() {
            return Err(AppError::Storage(format!(
                "wiki node has incomplete YAML frontmatter: {}: missing {}",
                path.display(),
                missing.join(", ")
            )));
        }
        // The contract allows exactly the nine node keys: extra keys fail
        // validation instead of drifting into the promoted library.
        let extra: Vec<String> = mapping
            .keys()
            .map(|key| match key {
                yaml_serde::Value::String(key) => key.clone(),
                other => format!("{other:?}"),
            })
            .filter(|key| !WIKI_NODE_KEYS.contains(&key.as_str()))
            .collect();
        if !extra.is_empty() {
            return Err(AppError::Storage(format!(
                "wiki node declares frontmatter keys outside the contract: {}: {}",
                path.display(),
                extra.join(", ")
            )));
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

    #[test]
    fn validate_staging_compares_against_the_preparation_baseline_not_the_live_root() {
        let (_tmp, storage, library_id) = fixture();
        storage
            .store_document(&library_id, "source.md", None, "# Source\n\nBody.")
            .unwrap();
        let job = storage.create_job(&library_id, JobKind::Ingest).unwrap();
        let (_staging, baseline) = storage.prepare_staging(&library_id, &job.job_id).unwrap();

        // A concurrent submission lands in the live raw/ (and a manual edit
        // rewrites purpose.md) while the job runs: validation compares the
        // staging copy against its preparation baseline and ignores the live
        // root entirely.
        let root = storage.library_root(&library_id).unwrap();
        fs::write(root.join("raw/late.md"), "# Late arrival").unwrap();
        fs::write(root.join("purpose.md"), "rewritten live").unwrap();
        storage
            .validate_staging(&library_id, &job.job_id, &baseline)
            .unwrap();
    }

    /// A node whose frontmatter carries exactly the nine contract keys.
    fn contract_node(extra: &str) -> String {
        format!(
            "---\nnode_id: n\ncanonical_name: N\nkind: concept\nsources:\n  - path: raw/source.md\n    locator: 第一条\nrelations:\n  depends_on: []\n  related_to: []\n  opposite_to: []\nclaim_type: observed\nconfidence: 1.0\ncreated_at: 2026-08-01T00:00:00Z\nupdated_at: 2026-08-01T00:00:00Z\n{extra}---\n\n# N\n"
        )
    }

    #[test]
    fn validate_staging_enforces_exactly_the_nine_contract_keys() {
        let (_tmp, storage, library_id) = fixture();
        storage
            .store_document(&library_id, "source.md", None, "# Source\n\nBody.")
            .unwrap();
        let job = storage.create_job(&library_id, JobKind::Ingest).unwrap();
        let (staging, baseline) = storage.prepare_staging(&library_id, &job.job_id).unwrap();

        // Exactly the contract keys pass.
        fs::write(staging.join("wiki/n.md"), contract_node("")).unwrap();
        storage
            .validate_staging(&library_id, &job.job_id, &baseline)
            .unwrap();

        // An extra key violates the node contract.
        fs::write(
            staging.join("wiki/n.md"),
            contract_node("extra_key: not in the contract\n"),
        )
        .unwrap();
        let error = storage
            .validate_staging(&library_id, &job.job_id, &baseline)
            .unwrap_err();
        assert!(error.to_string().contains("extra_key"), "{error}");

        // A missing key as well.
        fs::write(
            staging.join("wiki/n.md"),
            contract_node("").replacen("confidence: 1.0\n", "", 1),
        )
        .unwrap();
        let error = storage
            .validate_staging(&library_id, &job.job_id, &baseline)
            .unwrap_err();
        assert!(error.to_string().contains("confidence"), "{error}");
    }

    #[test]
    fn validate_staging_rejects_every_modification_inside_staging() {
        let (_tmp, storage, library_id) = fixture();
        storage
            .store_document(&library_id, "source.md", None, "# Source\n\nBody.")
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

        // Restored byte-for-byte: the digests match again.
        storage
            .validate_staging(&library_id, &job.job_id, &baseline)
            .unwrap();
    }
}
