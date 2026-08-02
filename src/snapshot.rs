//! Library snapshot import/export behind the HTTP API and the `noema-cli`
//! command-line client.
//!
//! A snapshot is a gzip-compressed tar archive of exactly one content
//! library: the raw sources, compiled wiki nodes (LLM-WIKI contract),
//! reviews, graphify artifacts, the `.opencode` project (skills/plugins),
//! the derived index and the per-library SQLite database (dedupe records,
//! node registry, content FTS). Importing a snapshot always creates a
//! brand-new, fully isolated library — fresh id, fresh root directory,
//! fresh control-plane row. Libraries never share files and never
//! cross-reference each other, so a base "regulation library" can be
//! exported once and imported independently by many users, who then add
//! their own regulations through the normal ingestion pipeline.

use std::{
    fs,
    io::{self, BufReader, Read, Write},
    path::{Component, Path, PathBuf},
};

use chrono::Utc;
use flate2::{Compression, read::GzDecoder, write::GzEncoder};
use serde::{Deserialize, Serialize};
use tar::{Archive, Builder, EntryType};
use walkdir::WalkDir;

use crate::{
    error::AppError,
    models::{CreateLibraryRequest, Library},
    storage::{Storage, copy_path},
};

/// Name of the manifest entry stored at the front of every snapshot.
pub const SNAPSHOT_MANIFEST: &str = "noema-snapshot.json";
const SNAPSHOT_FORMAT: &str = "noema-library-snapshot";
const SNAPSHOT_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotManifest {
    pub format: String,
    pub version: u32,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    pub source_library_id: String,
    pub exported_at: String,
}

/// Export one content library (selected by id or, if unique, by name) as a
/// snapshot archive. Only the library's own root directory is walked; the
/// staging workspace, SQLite sidecar files, `.opencode/node_modules` and all
/// symbolic links are excluded so the archive is a portable, self-contained
/// copy with no references outside the library.
pub fn export_library(data_dir: &Path, selector: &str, output: &Path) -> Result<Library, AppError> {
    let storage = Storage::open(data_dir)?;
    let library = storage.resolve_library(selector)?;
    let root = PathBuf::from(&library.root);
    let file = fs::File::create(output)?;
    let encoder = GzEncoder::new(file, Compression::default());
    let mut archive = Builder::new(encoder);
    append_manifest(
        &mut archive,
        &SnapshotManifest {
            format: SNAPSHOT_FORMAT.into(),
            version: SNAPSHOT_VERSION,
            name: library.name.clone(),
            description: library.description.clone(),
            source_library_id: library.id.clone(),
            exported_at: Utc::now().to_rfc3339(),
        },
    )?;
    append_tree(&mut archive, &root)?;
    let encoder = archive.into_inner()?;
    encoder.finish()?;
    Ok(library)
}

/// Import a snapshot archive as a brand-new library. The archive is first
/// unpacked into a scratch directory under strict path validation, then
/// copied over a freshly scaffolded library; every derived artifact (index,
/// FTS, manifest) is regenerated from the copied tree so the new library is
/// internally consistent. Any failure rolls the library back completely.
pub fn import_library(
    archive_path: &Path,
    name: Option<&str>,
    description: Option<&str>,
    data_dir: &Path,
) -> Result<Library, AppError> {
    let storage = Storage::open(data_dir)?;
    // The scratch tree is removed by TempDir's Drop on every exit path.
    let scratch = tempfile::Builder::new()
        .prefix("import-")
        .tempdir_in(data_dir.join("jobs"))?;
    import_inner(&storage, archive_path, name, description, scratch.path())
}

fn import_inner(
    storage: &Storage,
    archive_path: &Path,
    name: Option<&str>,
    description: Option<&str>,
    scratch: &Path,
) -> Result<Library, AppError> {
    let file = fs::File::open(archive_path)?;
    let mut archive = Archive::new(GzDecoder::new(BufReader::new(file)));
    unpack_validated(&mut archive, scratch)?;

    let manifest = read_snapshot_manifest(scratch)?;
    let had_opencode = scratch.join(".opencode").is_dir();
    // The snapshot records the source library's name; importers keep it
    // unless they pass an explicit one. Names are unique, so re-importing
    // a snapshot of a library that still exists requires a new name.
    let name = name
        .map(str::to_string)
        .or_else(|| manifest.as_ref().map(|item| item.name.clone()))
        .unwrap_or_else(|| default_name_from_archive(archive_path));
    let description = description
        .map(str::to_string)
        .or_else(|| manifest.and_then(|item| item.description));

    let library = storage.create_library(&CreateLibraryRequest { name, description })?;
    let root = PathBuf::from(&library.root);
    if let Err(error) = overlay_and_repair(storage, &library, &root, scratch, had_opencode) {
        storage.discard_on_failure(&library.id, "rejected snapshot import");
        return Err(error);
    }
    Ok(library)
}

fn overlay_and_repair(
    storage: &Storage,
    library: &Library,
    root: &Path,
    scratch: &Path,
    had_opencode: bool,
) -> Result<(), AppError> {
    copy_path(scratch, root)?;
    // Regenerate index.md, the content FTS and manifest.json from the copied
    // tree and database so every derived artifact matches the snapshot.
    storage.rebuild_index(&library.id)?;
    // Snapshots ship their own .opencode project; only archives without one
    // need the installer to become queryable.
    if !had_opencode {
        crate::bootstrap::install_graphify(root)?;
    }
    // Always refresh the four Noema skills to this binary's versions so an
    // imported library follows the current node contract.
    crate::bootstrap::write_skills(root)
}

fn append_manifest<W: Write>(
    archive: &mut Builder<W>,
    manifest: &SnapshotManifest,
) -> Result<(), AppError> {
    let bytes = serde_json::to_vec_pretty(manifest)?;
    let mut header = tar::Header::new_gnu();
    header.set_size(bytes.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    archive.append_data(&mut header, SNAPSHOT_MANIFEST, &bytes[..])?;
    Ok(())
}

fn append_tree<W: Write>(archive: &mut Builder<W>, root: &Path) -> Result<(), AppError> {
    for entry in WalkDir::new(root).follow_links(false).sort_by_file_name() {
        let entry = entry?;
        let relative = entry.path().strip_prefix(root)?;
        if relative.as_os_str().is_empty() || is_excluded(relative) {
            continue;
        }
        // Isolation: never archive links; a snapshot contains only regular
        // files and directories from inside this one library.
        let file_type = entry.file_type();
        if file_type.is_dir() {
            archive.append_dir(relative, entry.path())?;
        } else if file_type.is_file() {
            archive.append_path_with_name(entry.path(), relative)?;
        }
    }
    Ok(())
}

/// Runtime state that never travels with a snapshot. `library.sqlite` itself
/// IS exported (portable: it only stores library-relative paths); only its
/// process-local sidecar files are excluded.
fn is_excluded(relative: &Path) -> bool {
    let mut components = relative.components();
    let Some(Component::Normal(first)) = components.next() else {
        return true;
    };
    let first = first.to_string_lossy();
    if first == "staging" {
        return true;
    }
    if first != "library.sqlite" && first.starts_with("library.sqlite") {
        return true;
    }
    if first == ".opencode"
        && matches!(components.next(), Some(Component::Normal(second)) if matches!(
            second.to_string_lossy().as_ref(),
            "node_modules" | "opencode-loop"
        ))
    {
        return true;
    }
    false
}

/// Unpack an archive into `destination`, rejecting every entry that is not a
/// plain relative path to a regular file or directory: absolute paths, `..`
/// components, symbolic links and hard links all fail the import outright.
fn unpack_validated<R: Read>(archive: &mut Archive<R>, destination: &Path) -> Result<(), AppError> {
    for entry in archive.entries()? {
        let mut entry = entry?;
        let entry_type = entry.header().entry_type();
        let relative = entry.path()?.into_owned();
        validate_relative_path(&relative)?;
        let target = destination.join(&relative);
        // Defence in depth: the joined path must stay inside the destination.
        if !target.starts_with(destination) {
            return Err(AppError::BadRequest(format!(
                "snapshot entry escapes the library root: {}",
                relative.display()
            )));
        }
        match entry_type {
            EntryType::Directory => {
                fs::create_dir_all(&target)?;
            }
            EntryType::Regular => {
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent)?;
                }
                let mut file = fs::File::create(&target)?;
                io::copy(&mut entry, &mut file)?;
            }
            other => {
                return Err(AppError::BadRequest(format!(
                    "snapshot entry type not allowed (byte {:?}): {}",
                    char::from(other.as_byte()),
                    relative.display()
                )));
            }
        }
    }
    Ok(())
}

fn validate_relative_path(path: &Path) -> Result<(), AppError> {
    let escaped = path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_) | Component::CurDir));
    if escaped {
        return Err(AppError::BadRequest(format!(
            "snapshot entry escapes the library root: {}",
            path.display()
        )));
    }
    Ok(())
}

fn read_snapshot_manifest(scratch: &Path) -> Result<Option<SnapshotManifest>, AppError> {
    let path = scratch.join(SNAPSHOT_MANIFEST);
    if !path.is_file() {
        return Ok(None);
    }
    let manifest: SnapshotManifest = serde_json::from_slice(&fs::read(&path)?)
        .map_err(|error| AppError::BadRequest(format!("invalid snapshot manifest: {error}")))?;
    if manifest.format != SNAPSHOT_FORMAT {
        return Err(AppError::BadRequest(format!(
            "unsupported snapshot format: {}",
            manifest.format
        )));
    }
    if manifest.version != SNAPSHOT_VERSION {
        return Err(AppError::BadRequest(format!(
            "unsupported snapshot version: {}",
            manifest.version
        )));
    }
    Ok(Some(manifest))
}

fn default_name_from_archive(path: &Path) -> String {
    let stem = path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("imported-library");
    let stem = stem.strip_suffix(".tar").unwrap_or(stem);
    if stem.is_empty() {
        "imported-library".to_string()
    } else {
        stem.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_path_validation_rejects_escapes() {
        assert!(validate_relative_path(Path::new("raw/a.md")).is_ok());
        assert!(validate_relative_path(Path::new("./raw/a.md")).is_ok());
        assert!(validate_relative_path(Path::new(".opencode/skills/kb-query/SKILL.md")).is_ok());
        assert!(validate_relative_path(Path::new("/etc/passwd")).is_err());
        assert!(validate_relative_path(Path::new("../evil.md")).is_err());
        assert!(validate_relative_path(Path::new("raw/../../evil.md")).is_err());
    }

    #[test]
    fn export_excludes_runtime_state_but_keeps_the_database() {
        assert!(is_excluded(Path::new("staging")));
        assert!(is_excluded(Path::new("staging/job-1/wiki/x.md")));
        assert!(is_excluded(Path::new("library.sqlite-wal")));
        assert!(is_excluded(Path::new("library.sqlite-shm")));
        assert!(is_excluded(Path::new(
            ".opencode/node_modules/left-pad/index.js"
        )));
        assert!(is_excluded(Path::new(".opencode/opencode-loop/ses_x.json")));
        assert!(!is_excluded(Path::new("library.sqlite")));
        assert!(!is_excluded(Path::new(
            ".opencode/skills/kb-query/SKILL.md"
        )));
        assert!(!is_excluded(Path::new("raw/a.md")));
        assert!(!is_excluded(Path::new("graphify-out/graph.json")));
        assert!(!is_excluded(Path::new(".graphifyignore")));
    }

    #[test]
    fn default_name_strips_the_tar_suffix() {
        assert_eq!(
            default_name_from_archive(Path::new("/tmp/base.tar.gz")),
            "base"
        );
        assert_eq!(default_name_from_archive(Path::new("snap.archive")), "snap");
        assert_eq!(
            default_name_from_archive(Path::new("/x/.tar.gz")),
            "imported-library"
        );
    }

    #[test]
    fn export_import_round_trip_is_an_isolated_full_copy() {
        let source_dir = tempfile::tempdir().unwrap();
        let target_dir = tempfile::tempdir().unwrap();
        let storage = Storage::open(source_dir.path()).unwrap();
        let library = storage
            .create_library(&CreateLibraryRequest {
                name: "法规库".into(),
                description: Some("基础通用法规".into()),
            })
            .unwrap();
        let root = PathBuf::from(&library.root);
        storage
            .store_document(&library.id, "regulation.md", None, "# 法规\n\n第一条。")
            .unwrap();
        fs::write(root.join("wiki/regulation.md"), "---\nnode_id: reg\n---\n").unwrap();
        fs::create_dir_all(root.join("graphify-out")).unwrap();
        fs::write(root.join("graphify-out/graph.json"), r#"{"nodes":[]}"#).unwrap();
        fs::create_dir_all(root.join(".opencode/skills/kb-query")).unwrap();
        fs::write(
            root.join(".opencode/skills/kb-query/SKILL.md"),
            "old bundled skill",
        )
        .unwrap();
        fs::create_dir_all(root.join("staging/job-x")).unwrap();
        fs::write(root.join("staging/job-x/junk.md"), "junk").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("/etc/passwd", root.join("raw/evil-link")).unwrap();

        let archive = source_dir.path().join("snap.tar.gz");
        export_library(source_dir.path(), &library.id, &archive).unwrap();

        let imported = import_library(&archive, Some("副本"), None, target_dir.path()).unwrap();
        assert_ne!(imported.id, library.id);
        assert!(
            imported
                .root
                .starts_with(&target_dir.path().display().to_string())
        );
        let imported_root = PathBuf::from(&imported.root);
        assert_eq!(
            fs::read_to_string(imported_root.join("wiki/regulation.md")).unwrap(),
            "---\nnode_id: reg\n---\n"
        );
        assert_eq!(
            fs::read_to_string(imported_root.join("graphify-out/graph.json")).unwrap(),
            r#"{"nodes":[]}"#
        );
        // The snapshot database carried the document record over.
        let connection = rusqlite::Connection::open(imported_root.join("library.sqlite")).unwrap();
        let documents: i64 = connection
            .query_row("SELECT COUNT(*) FROM documents", [], |row| row.get(0))
            .unwrap();
        assert_eq!(documents, 1);
        // Runtime state and links never travel.
        assert!(!imported_root.join("staging/job-x").exists());
        assert!(!imported_root.join("raw/evil-link").exists());
        // Noema skills were refreshed to this binary's contract.
        let skill =
            fs::read_to_string(imported_root.join(".opencode/skills/kb-query/SKILL.md")).unwrap();
        assert!(skill.contains("noema-answer"));
    }

    /// The `tar` crate's own builder refuses `..` paths, but archives from
    /// other tools (GNU tar, python tarfile) may contain them. Craft a raw
    /// 512-byte POSIX header to prove unpack validation rejects those too.
    fn malicious_archive_bytes(path: &str, payload: &[u8]) -> Vec<u8> {
        let mut header = [0u8; 512];
        header[..path.len()].copy_from_slice(path.as_bytes());
        header[100..108].copy_from_slice(b"0000644\0");
        header[124..136].copy_from_slice(format!("{:011o}\0", payload.len()).as_bytes());
        header[156] = b'0'; // regular file
        header[257..263].copy_from_slice(b"ustar\0");
        header[263..265].copy_from_slice(b"00");
        header[148..156].copy_from_slice(b"        "); // checksum field as spaces
        let checksum: u32 = header.iter().map(|&byte| u32::from(byte)).sum();
        header[148..156].copy_from_slice(format!("{checksum:06o}\0 ").as_bytes());

        let mut bytes = Vec::new();
        bytes.extend_from_slice(&header);
        bytes.extend_from_slice(payload);
        bytes.extend(std::iter::repeat_n(0u8, (512 - payload.len() % 512) % 512));
        bytes.extend_from_slice(&[0u8; 1024]); // end-of-archive marker
        bytes
    }

    #[test]
    fn unpack_rejects_dotdot_entries_from_foreign_archives() {
        let scratch = tempfile::tempdir().unwrap();
        let bytes = malicious_archive_bytes("../evil.md", b"pwned");
        let mut archive = Archive::new(&bytes[..]);
        let error = unpack_validated(&mut archive, scratch.path()).unwrap_err();
        assert!(error.to_string().contains("escapes"), "{error}");
        assert!(!scratch.path().parent().unwrap().join("evil.md").exists());
    }

    #[test]
    fn import_rejects_archives_containing_links() {
        let workspace = tempfile::tempdir().unwrap();
        let archive_path = workspace.path().join("links.tar.gz");
        {
            let file = fs::File::create(&archive_path).unwrap();
            let encoder = GzEncoder::new(file, Compression::default());
            let mut archive = Builder::new(encoder);
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(EntryType::Symlink);
            header.set_mode(0o777);
            header.set_size(0);
            header.set_cksum();
            archive
                .append_link(&mut header, "evil-link", "/etc/passwd")
                .unwrap();
            archive.into_inner().unwrap().finish().unwrap();
        }
        let data_dir = workspace.path().join("data");
        let error = import_library(&archive_path, None, None, &data_dir).unwrap_err();
        assert!(error.to_string().contains("not allowed"), "{error}");
        // A rejected import leaves no library row behind.
        let storage = Storage::open(&data_dir).unwrap();
        assert!(storage.list_libraries().unwrap().is_empty());
    }
}
