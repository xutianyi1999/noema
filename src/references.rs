//! Safety for library knowledge paths: anything cited, served or promoted
//! under `raw/` or `wiki/` must stay inside the library tree.

use std::path::{Component, Path};

/// A library-relative knowledge path: under `raw/` or `wiki/`, never
/// absolute, never escaping the library tree.
pub(crate) fn safe_knowledge_path(path: &str) -> bool {
    (path.starts_with("raw/") || path.starts_with("wiki/"))
        && !Path::new(path).is_absolute()
        && Path::new(path).components().all(|component| {
            !matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
}

#[cfg(test)]
mod tests {
    use super::safe_knowledge_path;

    #[test]
    fn knowledge_paths_must_stay_inside_raw_or_wiki() {
        assert!(safe_knowledge_path("raw/担保法.md"));
        assert!(safe_knowledge_path("wiki/concept.md"));
        assert!(safe_knowledge_path("raw/sub/dir/source.txt"));
        assert!(!safe_knowledge_path("library.sqlite"));
        assert!(!safe_knowledge_path(".opencode/skills/kb-query/SKILL.md"));
        assert!(!safe_knowledge_path("raw/../library.sqlite"));
        assert!(!safe_knowledge_path("../other-library/raw/a.md"));
        assert!(!safe_knowledge_path("/etc/passwd"));
    }
}
