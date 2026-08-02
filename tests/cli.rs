//! End-to-end coverage of the `noema-cli` client against a real `noema`
//! server process: every operation travels the HTTP API, the same path used
//! when client and server run on different machines. The server spawns its
//! own OpenCode Server child on a free port and runs the graphify installer
//! on library creation, so `opencode` and `graphify` must be on PATH — but
//! no model or network access is needed (create/export/import never invoke
//! the Agent).

use std::{
    fs,
    net::TcpStream,
    path::Path,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

struct Server {
    child: Child,
    base: String,
}

fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

fn spawn_server(data_dir: &Path) -> Server {
    let port = free_port();
    let child = Command::new(env!("CARGO_BIN_EXE_noema"))
        .arg("--data-dir")
        .arg(data_dir)
        .arg("--bind")
        .arg(format!("127.0.0.1:{port}"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    // Startup includes spawning the OpenCode Server child. Both tests in
    // this binary run in parallel and each spawns one, so a cold node
    // runtime can lose a CPU race; allow generous time (serial startup is
    // ~1s).
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            break;
        }
        assert!(Instant::now() < deadline, "server did not come up");
        std::thread::sleep(Duration::from_millis(50));
    }
    Server {
        child,
        base: format!("http://127.0.0.1:{port}"),
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        // SIGINT (not SIGKILL) lets noema run its shutdown path and stop the
        // OpenCode Server child it spawned — SIGKILL would orphan that child,
        // since it runs in its own process group.
        let pid = self.child.id().to_string();
        let _ = Command::new("kill").args(["-s", "INT", &pid]).status();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                return;
            }
            if Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The client binary pointed at the test server. reqwest honours `no_proxy`
/// (the ambient http_proxy in this environment hijacks localhost).
fn client(base: &str) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_noema-cli"));
    command
        .arg("--server")
        .arg(base)
        .env("no_proxy", "127.0.0.1,localhost")
        .env("NO_PROXY", "127.0.0.1,localhost");
    command
}

fn run(command: &mut Command) -> (bool, String, String) {
    let output = command.output().unwrap();
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// Read one value out of the CLI's `key  value` output blocks, e.g. the
/// `ID  法规库` row that `create` and `import` print.
fn field<'a>(stdout: &'a str, key: &str) -> &'a str {
    stdout
        .lines()
        .find_map(|line| {
            let mut tokens = line.split_whitespace();
            (tokens.next() == Some(key))
                .then(|| tokens.next())
                .flatten()
        })
        .unwrap()
}

#[test]
fn cli_drives_the_http_api_through_an_export_import_round_trip() {
    let workspace = tempfile::tempdir().unwrap();
    let data_dir = workspace.path().join("data");
    let server = spawn_server(&data_dir);

    // Create a library over HTTP.
    let (ok, stdout, stderr) = run(client(&server.base).args(["create", "法规库"]));
    assert!(ok, "{stderr}");
    let library_id = field(&stdout, "ID").to_string();

    // Give the library some knowledge. The test can reach the server's data
    // directory directly; a remote client could not — it only speaks HTTP.
    let root = data_dir.join("libraries").join(&library_id);
    fs::write(
        root.join("wiki/regulation.md"),
        "---\nnode_id: reg\n---\nbody",
    )
    .unwrap();

    // The library shows up in the listing.
    let (ok, stdout, _) = run(client(&server.base).arg("list"));
    assert!(ok);
    assert!(stdout.contains(&library_id), "{stdout}");

    // Export by unique name; the snapshot is downloaded to a local file.
    let archive = workspace.path().join("snap.tar.gz");
    let (ok, _, stderr) = run(client(&server.base)
        .args(["export", "法规库"])
        .arg("-o")
        .arg(&archive));
    assert!(ok, "{stderr}");
    let bytes = fs::read(&archive).unwrap();
    assert_eq!(&bytes[..2], &[0x1f, 0x8b], "export must be gzip");

    // Import travels back over HTTP and always creates a fresh library.
    let (ok, stdout, stderr) = run(client(&server.base)
        .arg("import")
        .arg(&archive)
        .args(["--name", "副本"]));
    assert!(ok, "{stderr}");
    let imported_id = field(&stdout, "ID").to_string();
    assert_ne!(imported_id, library_id);
    assert!(stdout.contains("副本"), "{stdout}");

    let imported_root = data_dir.join("libraries").join(&imported_id);
    assert_eq!(
        fs::read_to_string(imported_root.join("wiki/regulation.md")).unwrap(),
        "---\nnode_id: reg\n---\nbody"
    );

    // Both libraries are listed afterwards.
    let (ok, stdout, _) = run(client(&server.base).arg("list"));
    assert!(ok);
    assert!(
        stdout.contains(&library_id) && stdout.contains(&imported_id),
        "{stdout}"
    );
}

#[test]
fn cli_import_rejects_a_hostile_archive_over_http() {
    use flate2::{Compression, write::GzEncoder};
    use tar::Builder;

    let workspace = tempfile::tempdir().unwrap();
    let server = spawn_server(&workspace.path().join("data"));

    let archive = workspace.path().join("evil.tar.gz");
    {
        let file = fs::File::create(&archive).unwrap();
        let mut builder = Builder::new(GzEncoder::new(file, Compression::default()));
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Symlink);
        header.set_mode(0o777);
        header.set_size(0);
        header.set_cksum();
        builder
            .append_link(&mut header, "evil-link", "/etc/passwd")
            .unwrap();
        builder.into_inner().unwrap().finish().unwrap();
    }

    let (ok, _, stderr) = run(client(&server.base).arg("import").arg(&archive));
    assert!(!ok);
    assert!(stderr.contains("not allowed"), "{stderr}");

    // The server rolled the rejected import back: nothing was created.
    let (ok, stdout, _) = run(client(&server.base).arg("list"));
    assert!(ok);
    assert!(stdout.contains("暂无内容库"), "{stdout}");
}
