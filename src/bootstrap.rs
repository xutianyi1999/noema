//! Shared library bootstrap: the upstream graphify installer plus the four
//! Noema skills. Both content-library creation (`service`) and snapshot
//! import (`snapshot`) use this single synchronous implementation; async
//! callers run it through `spawn_blocking`.

use std::{
    fs,
    io::Read,
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant},
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

/// The installer is an offline file-copying tool; anything taking longer
/// than this is wedged and gets killed instead of parking a blocking thread
/// forever.
const INSTALL_TIMEOUT: Duration = Duration::from_secs(120);
/// Installer diagnostics beyond this are noise; keep a bounded tail for the
/// error message without buffering a chatty release wholesale.
const STDERR_CAP: u64 = 16 * 1024;

/// Run the upstream graphify installer in one library project (offline;
/// writes the `.opencode/` plugins/skills/config and `AGENTS.md`).
pub(crate) fn install_graphify(root: &Path) -> Result<(), AppError> {
    let mut child = Command::new("graphify")
        .args(["install", "--platform", "opencode", "--project"])
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| AppError::Runtime(format!("unable to run graphify installer: {error}")))?;
    // Drain stderr on a helper thread (bounded): an undrained pipe would let
    // the installer block once the OS buffer fills.
    let stderr = child.stderr.take().expect("stderr is piped");
    let reader = std::thread::spawn(|| {
        let mut bytes = Vec::new();
        let _ = stderr.take(STDERR_CAP).read_to_end(&mut bytes);
        String::from_utf8_lossy(&bytes).into_owned()
    });
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if started.elapsed() > INSTALL_TIMEOUT {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(AppError::Runtime(format!(
                        "graphify installer timed out after {} s",
                        INSTALL_TIMEOUT.as_secs()
                    )));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(error) => {
                return Err(AppError::Runtime(format!(
                    "graphify installer error: {error}"
                )));
            }
        }
    };
    let stderr = reader.join().unwrap_or_default();
    if !status.success() {
        return Err(AppError::Runtime(format!(
            "graphify installer failed: {stderr}"
        )));
    }
    Ok(())
}

/// Refresh the four Noema skills and the generated contract block in
/// `AGENTS.md` to this binary's versions.
pub(crate) fn write_skills(root: &Path) -> Result<(), AppError> {
    for (relative, contents) in skill_files() {
        let path = root.join(".opencode").join("skills").join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, contents)?;
    }
    write_agents_contract(root)
}

const CONTRACT_BEGIN: &str = "<!-- noema-contract -->";
const CONTRACT_END: &str = "<!-- /noema-contract -->";

/// Idempotently install the generated Noema contract (ingest discipline +
/// query contract with the schema and example) into the project's
/// `AGENTS.md`. OpenCode injects `AGENTS.md` into the system prompt of every
/// session, so the contract reaches the Agent as system-level instruction
/// and the per-query / per-ingest user messages carry no policy text. The
/// block is delimited by markers and replaced in place on refresh; anything
/// else in the file (e.g. the graphify installer's content) is preserved.
pub(crate) fn write_agents_contract(root: &Path) -> Result<(), AppError> {
    let path = root.join("AGENTS.md");
    let existing = fs::read_to_string(&path).unwrap_or_default();
    let block = format!(
        "{CONTRACT_BEGIN}\n{}\n{CONTRACT_END}",
        crate::answer::agents_contract()
    );
    let updated = match (existing.find(CONTRACT_BEGIN), existing.find(CONTRACT_END)) {
        (Some(begin), Some(end)) if end > begin => format!(
            "{}{}{}",
            &existing[..begin],
            block,
            &existing[end + CONTRACT_END.len()..]
        ),
        _ => {
            let trimmed = existing.trim_end();
            if trimmed.is_empty() {
                format!("{block}\n")
            } else {
                format!("{trimmed}\n\n{block}\n")
            }
        }
    };
    fs::write(path, updated)?;
    Ok(())
}

/// Full bootstrap for a brand-new library project: installer, then skills.
pub(crate) fn bootstrap(root: &Path) -> Result<(), AppError> {
    install_graphify(root)?;
    write_skills(root)
}
