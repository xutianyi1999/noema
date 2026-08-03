//! Shared library bootstrap: the upstream graphify installer plus the four
//! Noema skills. Both content-library creation (`service`) and snapshot
//! import (`snapshot`) use this single synchronous implementation; async
//! callers run it through `spawn_blocking`.

use std::{
    fs,
    io::{self, Read},
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
/// The graphify installer's project-scope skill for OpenCode. Its presence
/// marks a library the installer has completed; startup re-runs the full
/// bootstrap where it is missing.
const GRAPHIFY_MARKER: &str = ".opencode/skills/graphify/SKILL.md";

/// Whether the upstream graphify installer has completed in this library
/// project.
pub(crate) fn graphify_installed(root: &Path) -> bool {
    root.join(GRAPHIFY_MARKER).is_file()
}
/// Installer diagnostics beyond this are noise; keep a bounded head for the
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
    // Drain stderr on a helper thread: an undrained pipe would let the
    // installer block once the OS buffer fills. Only the head is kept for
    // the error message, but the drain itself never stops — a reader that
    // retires at the cap would block a verbose child on a full pipe and
    // wedge it until the timeout kills it.
    let stderr = child.stderr.take().expect("stderr is piped");
    let reader = std::thread::spawn(|| {
        let mut stderr = stderr;
        let mut bytes = Vec::new();
        let _ = io::copy(&mut stderr.by_ref().take(STDERR_CAP), &mut bytes);
        let _ = io::copy(&mut stderr, &mut io::sink());
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
                // Same hygiene as the timeout path: do not leave the child
                // running or the stderr reader unjoined.
                let _ = child.kill();
                let _ = child.wait();
                let stderr = reader.join().unwrap_or_default();
                return Err(AppError::Runtime(format!(
                    "graphify installer error: {error} {stderr}"
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
/// Heading of the always-on section the graphify installer writes into the
/// project's `AGENTS.md`.
const GRAPHIFY_SECTION_HEADING: &str = "## graphify";

/// Idempotently install the generated Noema contract (ingest discipline +
/// query contract with the schema and example) into the project's
/// `AGENTS.md`. OpenCode injects `AGENTS.md` into the system prompt of every
/// session, so the contract reaches the Agent as system-level instruction
/// and the per-query / per-ingest user messages carry no policy text. The
/// block is delimited by markers and replaced in place on refresh; anything
/// else in the file (e.g. the graphify installer's content) is preserved.
pub(crate) fn write_agents_contract(root: &Path) -> Result<(), AppError> {
    let path = root.join("AGENTS.md");
    let existing = strip_graphify_section(&fs::read_to_string(&path).unwrap_or_default());
    let block = format!(
        "{CONTRACT_BEGIN}\n{}\n{CONTRACT_END}",
        crate::answer::agents_contract()
    );
    let begin = existing.find(CONTRACT_BEGIN);
    // The closing marker only counts after the opening one: an orphaned end
    // marker earlier in the file (hand edit, merge conflict) must not pair
    // with the block, or every refresh would append a duplicate instead of
    // splicing.
    let end = begin.and_then(|begin| {
        let tail = begin + CONTRACT_BEGIN.len();
        existing[tail..]
            .find(CONTRACT_END)
            .map(|offset| tail + offset)
    });
    let updated = match (begin, end) {
        (Some(begin), Some(end)) => format!(
            "{}{}{}",
            &existing[..begin],
            block,
            &existing[end + CONTRACT_END.len()..]
        ),
        // Opening marker without a closing one: the previous write was cut
        // off mid-contract, so everything from the marker on is residue.
        (Some(begin), None) => format!("{}{block}\n", &existing[..begin]),
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

/// Drop the graphify installer's always-on section from `AGENTS.md` content.
/// That block tells every session to run `graphify query` first on content
/// questions and a bare `graphify update .` after modifications — a
/// code-repo stance that contradicts Noema's summaries-first query contract
/// and the skill-driven `/graphify . --update` ingest flow. The Noema
/// contract plus the upstream Skill (loaded on demand, per the contract)
/// already carry the library's whole graphify stance, so the installer's
/// voice is removed on every contract refresh; only a re-run installer
/// (fresh bootstrap) brings it back, and the next refresh strips it again.
fn strip_graphify_section(content: &str) -> String {
    let lines = content.lines().collect::<Vec<_>>();
    let Some(start) = lines
        .iter()
        .position(|line| *line == GRAPHIFY_SECTION_HEADING)
    else {
        return content.to_string();
    };
    // The section runs to the next heading, the contract marker or EOF.
    let end = lines[start + 1..]
        .iter()
        .position(|line| line.starts_with("## ") || *line == CONTRACT_BEGIN)
        .map_or(lines.len(), |offset| start + 1 + offset);
    let mut kept = lines[..start]
        .iter()
        .chain(&lines[end..])
        .copied()
        .collect::<Vec<_>>()
        .join("\n");
    if content.ends_with('\n') {
        kept.push('\n');
    }
    kept
}

/// Full bootstrap for a brand-new library project: installer, then skills.
pub(crate) fn bootstrap(root: &Path) -> Result<(), AppError> {
    install_graphify(root)?;
    write_skills(root)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contract_refresh_splices_the_existing_block_in_place() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("AGENTS.md");
        fs::write(
            &path,
            format!("preamble\n{CONTRACT_BEGIN}\nold contract\n{CONTRACT_END}\npostamble\n"),
        )
        .unwrap();
        write_agents_contract(tmp.path()).unwrap();
        let contents = fs::read_to_string(&path).unwrap();
        assert!(contents.starts_with("preamble\n"));
        assert!(contents.ends_with("postamble\n"));
        assert_eq!(contents.matches(CONTRACT_BEGIN).count(), 1, "{contents}");
        assert!(!contents.contains("old contract"), "{contents}");
    }

    #[test]
    fn contract_refresh_converges_despite_orphaned_markers() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("AGENTS.md");
        // A botched edit left the closing marker ahead of the opening one;
        // pairing them blindly appended a duplicate block on every refresh.
        fs::write(
            &path,
            format!("preamble\n{CONTRACT_END}\nstale\n{CONTRACT_BEGIN}\nold contract\n"),
        )
        .unwrap();
        write_agents_contract(tmp.path()).unwrap();
        let once = fs::read_to_string(&path).unwrap();
        write_agents_contract(tmp.path()).unwrap();
        let twice = fs::read_to_string(&path).unwrap();
        assert_eq!(once, twice, "refresh must be idempotent");
        assert_eq!(once.matches(CONTRACT_BEGIN).count(), 1, "{once}");
        assert!(once.contains("preamble"), "{once}");
        assert!(!once.contains("old contract"), "{once}");
    }

    /// The installer's always-on block (`## graphify` … graph-first rules,
    /// bare `graphify update .`) shares every session's system prompt with
    /// the Noema contract and contradicts it, so each refresh removes it.
    #[test]
    fn contract_refresh_strips_the_graphify_installers_always_on_block() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("AGENTS.md");
        fs::write(
            &path,
            "## graphify\n\nThis project has a knowledge graph at graphify-out/.\n\nRules:\n- For codebase questions, first run `graphify query \"<question>\"`.\n- After modifying code, run `graphify update .` to keep the graph current.\n",
        )
        .unwrap();
        write_agents_contract(tmp.path()).unwrap();
        let contents = fs::read_to_string(&path).unwrap();
        assert!(!contents.contains("## graphify"), "{contents}");
        assert!(!contents.contains("graphify update ."), "{contents}");
        assert_eq!(contents.matches(CONTRACT_BEGIN).count(), 1, "{contents}");
        // A second refresh is a no-op.
        write_agents_contract(tmp.path()).unwrap();
        assert_eq!(contents, fs::read_to_string(&path).unwrap());
    }

    #[test]
    fn stripping_the_graphify_block_preserves_other_sections() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("AGENTS.md");
        fs::write(
            &path,
            "preamble\n\n## graphify\n\ninstaller rules\n\n## Other section\n\nkept\n",
        )
        .unwrap();
        write_agents_contract(tmp.path()).unwrap();
        let contents = fs::read_to_string(&path).unwrap();
        assert!(contents.starts_with("preamble\n"), "{contents}");
        assert!(
            contents.contains("## Other section\n\nkept\n"),
            "{contents}"
        );
        assert!(!contents.contains("installer rules"), "{contents}");
        assert_eq!(contents.matches(CONTRACT_BEGIN).count(), 1, "{contents}");
    }

    #[test]
    fn contract_refresh_recovers_from_a_truncated_block() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("AGENTS.md");
        fs::write(
            &path,
            format!("preamble\n{CONTRACT_BEGIN}\nhalf-written contract"),
        )
        .unwrap();
        write_agents_contract(tmp.path()).unwrap();
        let contents = fs::read_to_string(&path).unwrap();
        assert_eq!(contents.matches(CONTRACT_BEGIN).count(), 1, "{contents}");
        assert_eq!(contents.matches(CONTRACT_END).count(), 1, "{contents}");
        assert!(!contents.contains("half-written"), "{contents}");
    }
}
