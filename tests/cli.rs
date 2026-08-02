//! Offline end-to-end coverage for the `noemactl` binary: list / export /
//! import round-trip and rejection of a malicious archive. Needs no OpenCode
//! server, model or graphify.

use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use noema::{models::CreateLibraryRequest, storage::Storage};

fn noemactl() -> Command {
    Command::new(env!("CARGO_BIN_EXE_noemactl"))
}

fn seed_library(data_dir: &Path) -> noema::models::Library {
    let storage = Storage::open(data_dir).unwrap();
    let library = storage
        .create_library(&CreateLibraryRequest {
            name: "regulations".into(),
            description: Some("base regulations".into()),
        })
        .unwrap();
    let root = PathBuf::from(&library.root);
    storage
        .store_document(
            &library.id,
            "regulation.md",
            None,
            "# Regulation\n\nArticle 1.",
        )
        .unwrap();
    fs::write(
        root.join("wiki/regulation.md"),
        "---\nnode_id: reg\n---\nbody",
    )
    .unwrap();
    fs::create_dir_all(root.join("graphify-out")).unwrap();
    fs::write(root.join("graphify-out/graph.json"), r#"{"nodes":[]}"#).unwrap();
    fs::create_dir_all(root.join(".opencode/skills/kb-query")).unwrap();
    fs::write(root.join(".opencode/skills/kb-query/SKILL.md"), "bundled").unwrap();
    library
}

#[test]
fn cli_export_import_round_trip_creates_an_isolated_copy() {
    let workspace = tempfile::tempdir().unwrap();
    let source = workspace.path().join("source");
    let target = workspace.path().join("target");
    let library = seed_library(&source);
    let archive = workspace.path().join("snapshot.tar.gz");

    let output = noemactl()
        .arg("--data-dir")
        .arg(&source)
        .arg("list")
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains(&library.id));

    let output = noemactl()
        .arg("--data-dir")
        .arg(&source)
        .arg("export")
        .arg(&library.id)
        .arg("-o")
        .arg(&archive)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(archive.is_file());

    let output = noemactl()
        .arg("--data-dir")
        .arg(&target)
        .arg("import")
        .arg(&archive)
        .arg("--name")
        .arg("regulations-copy")
        .env("NOEMA_INSTALL_GRAPHIFY", "0")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("imported"));

    // Exactly one library in the target data dir, fully distinct from source.
    let target_storage = Storage::open(&target).unwrap();
    let libraries = target_storage.list_libraries().unwrap();
    assert_eq!(libraries.len(), 1);
    assert_eq!(libraries[0].name, "regulations-copy");
    assert_ne!(libraries[0].id, library.id);
    let imported_root = PathBuf::from(&libraries[0].root);
    assert_eq!(
        fs::read_to_string(imported_root.join("wiki/regulation.md")).unwrap(),
        "---\nnode_id: reg\n---\nbody"
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

    // The source library is untouched.
    let source_storage = Storage::open(&source).unwrap();
    assert_eq!(source_storage.list_libraries().unwrap().len(), 1);
}

#[test]
fn cli_rejects_an_archive_containing_links() {
    use flate2::{Compression, write::GzEncoder};
    use tar::Builder;

    let workspace = tempfile::tempdir().unwrap();
    let data_dir = workspace.path().join("data");
    let archive_path = workspace.path().join("evil.tar.gz");
    {
        let file = fs::File::create(&archive_path).unwrap();
        let mut archive = Builder::new(GzEncoder::new(file, Compression::default()));
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Symlink);
        header.set_mode(0o777);
        header.set_size(0);
        header.set_cksum();
        archive
            .append_link(&mut header, "evil-link", "/etc/passwd")
            .unwrap();
        archive.into_inner().unwrap().finish().unwrap();
    }

    let output = noemactl()
        .arg("--data-dir")
        .arg(&data_dir)
        .arg("import")
        .arg(&archive_path)
        .env("NOEMA_INSTALL_GRAPHIFY", "0")
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("not allowed"), "{stderr}");
    assert!(
        Storage::open(&data_dir)
            .unwrap()
            .list_libraries()
            .unwrap()
            .is_empty()
    );
}
