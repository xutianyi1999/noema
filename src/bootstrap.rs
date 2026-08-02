//! Shared library bootstrap: the upstream graphify installer plus the four
//! Noema skills. Both content-library creation (`service`) and snapshot
//! import (`snapshot`) use this single synchronous implementation; async
//! callers run it through `spawn_blocking`.

use std::{
    fs,
    path::Path,
    process::{Command, Stdio},
};

use crate::error::AppError;

/// The four Noema skills (path relative to `.opencode/skills/` + contents).
pub(crate) fn skill_files() -> [(&'static str, &'static str); 4] {
    [
        (
            "kb-ingest/SKILL.md",
            include_str!("../.opencode/skills/kb-ingest/SKILL.md"),
        ),
        (
            "kb-query/SKILL.md",
            include_str!("../.opencode/skills/kb-query/SKILL.md"),
        ),
        (
            "kb-maintain/SKILL.md",
            include_str!("../.opencode/skills/kb-maintain/SKILL.md"),
        ),
        (
            "knowledge-compiler/SKILL.md",
            include_str!("../.opencode/skills/knowledge-compiler/SKILL.md"),
        ),
    ]
}

/// Run the upstream graphify installer in one library project (offline;
/// writes the `.opencode/` plugins/skills/config and `AGENTS.md`).
pub(crate) fn install_graphify(root: &Path) -> Result<(), AppError> {
    let output = Command::new("graphify")
        .args(["install", "--platform", "opencode", "--project"])
        .current_dir(root)
        .stdin(Stdio::null())
        .output()
        .map_err(|error| AppError::Runtime(format!("unable to run graphify installer: {error}")))?;
    if !output.status.success() {
        return Err(AppError::Runtime(format!(
            "graphify installer failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(())
}

/// Refresh the four Noema skills to this binary's versions.
pub(crate) fn write_skills(root: &Path) -> Result<(), AppError> {
    for (relative, contents) in skill_files() {
        let path = root.join(".opencode").join("skills").join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, contents)?;
    }
    Ok(())
}

/// Full bootstrap for a brand-new library project: installer, then skills.
pub(crate) fn bootstrap(root: &Path) -> Result<(), AppError> {
    install_graphify(root)?;
    write_skills(root)
}
