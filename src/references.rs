//! Extracts `raw/` and `wiki/` citations from an agent's answer text.

use std::path::{Component, Path};

use crate::models::Reference;

/// Every existing `raw/…` or `wiki/…` knowledge file cited in an agent
/// answer, in order of first citation, deduplicated by source path. For a
/// cited `raw/` source, `node` carries the matching `wiki/` node when one
/// exists.
pub(crate) fn extract_references(root: &Path, answer: &str) -> Vec<Reference> {
    let mut references = Vec::new();
    // Real answers cite paths with locators and mixed punctuation, e.g.
    // `raw/abc-source.md:5-7,15；wiki/concept.md:24）`. Take the maximal
    // run of path-safe characters at every `raw/` / `wiki/` occurrence
    // instead of splitting on whitespace.
    let mut occurrences: Vec<(usize, &str)> = ["raw/", "wiki/"]
        .iter()
        .flat_map(|prefix| {
            answer
                .match_indices(prefix)
                .map(|(start, _)| (start, *prefix))
        })
        .collect();
    occurrences.sort_unstable_by_key(|(start, _)| *start);
    for (start, prefix) in occurrences {
        let Some(candidate) = reference_candidate(answer, start) else {
            continue;
        };
        let path = root.join(&candidate);
        if is_safe_reference(&candidate, prefix)
            && path.is_file()
            && !references
                .iter()
                .any(|item: &Reference| item.source == candidate)
        {
            let title = Path::new(&candidate)
                .file_stem()
                .and_then(|value| value.to_str())
                .unwrap_or(&candidate)
                .to_string();
            let node = if candidate.starts_with("raw/") {
                let stem = Path::new(&candidate)
                    .file_stem()
                    .and_then(|value| value.to_str())
                    .unwrap_or_default();
                let node = format!("wiki/{stem}.md");
                root.join(&node).is_file().then_some(node)
            } else {
                None
            };
            references.push(Reference {
                title,
                source: candidate,
                node,
            });
        }
    }
    references
}

/// Extracts a `raw/…` or `wiki/…` file path starting at `start`, stopping at
/// the first character that is neither alphanumeric (Unicode, so CJK
/// filenames stay intact) nor one of `/._-`, and dropping any trailing dots.
/// Line/column locators (`:24`, `:5-7,15`), whitespace and surrounding
/// punctuation (ASCII or full-width) are therefore excluded. Returns `None`
/// unless the run ends in an allowed knowledge-file extension.
fn reference_candidate(text: &str, start: usize) -> Option<String> {
    let run: String = text[start..]
        .chars()
        .take_while(|character| {
            character.is_alphanumeric() || matches!(character, '/' | '.' | '_' | '-')
        })
        .collect();
    let candidate = run.trim_end_matches('.');
    (candidate.ends_with(".md") || candidate.ends_with(".txt")).then(|| candidate.to_string())
}

fn is_safe_reference(candidate: &str, prefix: &str) -> bool {
    candidate.starts_with(prefix)
        && !Path::new(candidate).is_absolute()
        && Path::new(candidate).components().all(|component| {
            !matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf};

    use super::*;

    fn library_with_files() -> (tempfile::TempDir, PathBuf) {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().to_path_buf();
        fs::create_dir_all(root.join("raw")).unwrap();
        fs::create_dir_all(root.join("wiki")).unwrap();
        fs::write(root.join("raw/source-a.md"), "a").unwrap();
        fs::write(root.join("raw/orphan.md"), "o").unwrap();
        fs::write(root.join("wiki/source-a.md"), "n").unwrap();
        fs::write(root.join("wiki/concept.md"), "c").unwrap();
        (directory, root)
    }

    #[test]
    fn extracts_citations_with_locators_and_cjk_punctuation() {
        let (_directory, root) = library_with_files();
        let references = extract_references(
            &root,
            "见 raw/source-a.md:5-7,15；与 wiki/concept.md:24）。",
        );
        let sources: Vec<&str> = references.iter().map(|item| item.source.as_str()).collect();
        assert_eq!(sources, ["raw/source-a.md", "wiki/concept.md"]);
        assert_eq!(references[0].title, "source-a");
        assert_eq!(references[0].node.as_deref(), Some("wiki/source-a.md"));
        assert_eq!(references[1].node, None);
    }

    #[test]
    fn dedupes_repeated_citations() {
        let (_directory, root) = library_with_files();
        let references = extract_references(&root, "raw/source-a.md 再看 raw/source-a.md:3");
        assert_eq!(references.len(), 1);
    }

    #[test]
    fn extracts_cjk_filenames_with_locators_and_punctuation() {
        let (_directory, root) = library_with_files();
        fs::write(root.join("raw/评估指南.md"), "e").unwrap();
        let references =
            extract_references(&root, "详见 raw/评估指南.md:5-7；另见 raw/评估指南.md。");
        let sources: Vec<&str> = references.iter().map(|item| item.source.as_str()).collect();
        assert_eq!(sources, ["raw/评估指南.md"]);
    }

    #[test]
    fn rejects_traversal_missing_files_and_foreign_extensions() {
        let (_directory, root) = library_with_files();
        assert!(extract_references(&root, "raw/../outside.md").is_empty());
        assert!(extract_references(&root, "raw/absent.md").is_empty());
        assert!(extract_references(&root, "raw/data.json").is_empty());
    }

    #[test]
    fn maps_raw_sources_to_wiki_nodes_only_when_present() {
        let (_directory, root) = library_with_files();
        let references = extract_references(&root, "raw/orphan.md");
        assert_eq!(references.len(), 1);
        assert_eq!(references[0].node, None);
    }
}
