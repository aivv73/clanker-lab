use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::Stdio;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use base64::Engine;
use rusqlite::Connection;

use super::{command, fixture_bin, refresh, snapshot_count};

const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

fn accept(listener: &TcpListener) -> TcpStream {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match listener.accept() {
            Ok((stream, _)) => return stream,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < deadline, "client never connected");
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(error) => panic!("{error}"),
        }
    }
}

fn request(stream: &mut TcpStream) -> String {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut bytes = Vec::new();
    while !bytes.ends_with(b"\r\n\r\n") {
        let mut byte = [0];
        stream.read_exact(&mut byte).unwrap();
        bytes.push(byte[0]);
        assert!(bytes.len() < 8192);
    }
    String::from_utf8(bytes).unwrap()
}

fn serve_recipe(stream: &mut TcpStream, recipe: &str) {
    let body = serde_json::to_vec(&serde_json::json!({
        "file_path": ".SRCINFO", "encoding": "base64", "commit_id": COMMIT,
        "content": base64::engine::general_purpose::STANDARD.encode(recipe)
    }))
    .unwrap();
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .unwrap();
    stream.write_all(&body).unwrap();
}

fn listener() -> (TcpListener, String) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let endpoint = format!("http://{}/api/v4", listener.local_addr().unwrap());
    (listener, endpoint)
}

#[test]
fn recipe_refresh_fetches_by_base_records_partial_results_and_preserves_origins() {
    let data = tempfile::tempdir().unwrap();
    let bin = fixture_bin();
    let (listener, endpoint) = listener();
    let server = std::thread::spawn(move || {
        let mut requests = Vec::new();
        for _ in 0..3 {
            let mut stream = accept(&listener);
            let request = request(&mut stream);
            if request.contains("%2Falpha/") {
                assert!(request.contains("ref=1.0-1"));
                serve_recipe(
                    &mut stream,
                    "pkgbase = alpha\n\tpkgver = 1.0\n\tpkgrel = 1\n\tsource = git+https://example.org/alpha.git#tag=v1.0\npkgname = alpha\n",
                );
            } else if request.contains("%2Fsuite/") {
                assert!(request.contains("ref=2.0-1"));
                serve_recipe(
                    &mut stream,
                    "pkgbase = suite\n\tpkgver = 2.0\n\tpkgrel = 1\n\tsource = https://example.org/suite-2.0.tar.gz\npkgname = suite-cli\npkgname = suite-docs\n",
                );
            } else {
                assert!(request.contains("%2Fforeign-tool/"));
                stream
                    .write_all(
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    )
                    .unwrap();
            }
            requests.push(request);
        }
        requests
    });
    let output = command(data.path())
        .args([
            "refresh",
            "--recipes-only",
            "--arch-api-url",
            &endpoint,
            "--format",
            "json",
        ])
        .env("PATH", bin.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert_eq!(server.join().unwrap().len(), 3);
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["installed_total"], 4);
    let database = Connection::open(data.path().join("paclang/paclang.db")).unwrap();
    let mut statement = database.prepare("SELECT package_name, outcome, evidence_grade, recipe_commit, failure_reason FROM provider_attempts ORDER BY package_name").unwrap();
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?,
            ))
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(rows.len(), 4);
    for index in [0, 2, 3] {
        assert_eq!(rows[index].1, "resolved");
        assert_eq!(rows[index].2.as_deref(), Some("reconstructed"));
        assert_eq!(rows[index].3.as_deref(), Some(COMMIT));
    }
    assert_eq!(rows[1].1, "unresolved");
    assert_eq!(rows[1].4.as_deref(), Some("recipe API returned HTTP 404"));
    assert_eq!(
        database
            .query_row("SELECT count(*) FROM analysis_targets", [], |row| row
                .get::<_, u32>(0))
            .unwrap(),
        2
    );
    assert_eq!(
        database
            .query_row(
                "SELECT count(*) FROM installed_packages WHERE origin = 'unknown'",
                [],
                |row| row.get::<_, u32>(0)
            )
            .unwrap(),
        4
    );
    assert_eq!(
        database
            .query_row(
                "SELECT count(*) FROM provider_attempts WHERE recorded_at != ''",
                [],
                |row| row.get::<_, u32>(0)
            )
            .unwrap(),
        4
    );
}

#[test]
fn malformed_metadata_is_stored_as_unresolved_with_recipe_provenance() {
    let data = tempfile::tempdir().unwrap();
    let bin = fixture_bin();
    let (listener, endpoint) = listener();
    let server = std::thread::spawn(move || {
        for _ in 0..3 {
            let mut stream = accept(&listener);
            request(&mut stream);
            serve_recipe(&mut stream, "invalid .SRCINFO");
        }
    });
    let output = command(data.path())
        .args(["refresh", "--recipes-only", "--arch-api-url", &endpoint])
        .env("PATH", bin.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    server.join().unwrap();
    let database = Connection::open(data.path().join("paclang/paclang.db")).unwrap();
    assert_eq!(database.query_row("SELECT count(*) FROM provider_attempts WHERE outcome = 'unresolved' AND recipe_commit = ?1", [COMMIT], |row| row.get::<_, u32>(0)).unwrap(), 4);
    assert_eq!(snapshot_count(data.path()), 1);
}

#[test]
fn interrupting_recipe_fetch_rolls_back_completed_attempts_and_keeps_snapshot() {
    let data = tempfile::tempdir().unwrap();
    let bin = fixture_bin();
    assert!(refresh(&[], data.path(), bin.path()).status.success());
    let (listener, endpoint) = listener();
    let (started_tx, started_rx) = mpsc::channel();
    let (finish_tx, finish_rx) = mpsc::channel();
    let server = std::thread::spawn(move || {
        let mut first = accept(&listener);
        request(&mut first);
        serve_recipe(&mut first, "invalid .SRCINFO");
        drop(first);
        let mut second = accept(&listener);
        request(&mut second);
        started_tx.send(()).unwrap();
        finish_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    });
    let child = command(data.path())
        .args([
            "refresh",
            "--recipes-only",
            "--arch-api-url",
            &endpoint,
            "--format",
            "json",
        ])
        .env("PATH", bin.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let start = Instant::now();
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(i32::try_from(child.id()).unwrap()),
        nix::sys::signal::Signal::SIGINT,
    )
    .unwrap();
    let output = child.wait_with_output().unwrap();
    finish_tx.send(()).unwrap();
    server.join().unwrap();
    assert_eq!(output.status.code(), Some(130));
    assert!(start.elapsed() < Duration::from_secs(1));
    assert_eq!(snapshot_count(data.path()), 1);
    let database = Connection::open(data.path().join("paclang/paclang.db")).unwrap();
    assert_eq!(
        database
            .query_row("SELECT count(*) FROM provider_attempts", [], |row| row
                .get::<_, u32>(0))
            .unwrap(),
        0
    );
}

#[test]
fn recipe_mode_is_explicit_and_conflicts_with_metadata_only() {
    for args in [
        vec!["refresh"],
        vec!["refresh", "--metadata-only", "--recipes-only"],
        vec![
            "refresh",
            "--metadata-only",
            "--arch-api-url",
            "https://example.org",
        ],
    ] {
        let data = tempfile::tempdir().unwrap();
        let output = command(data.path()).args(args).output().unwrap();
        assert_eq!(output.status.code(), Some(2));
        assert!(!data.path().join("paclang/paclang.db").exists());
    }
}

#[test]
fn population_filters_limit_fetching_without_truncating_inventory() {
    let data = tempfile::tempdir().unwrap();
    let bin = fixture_bin();
    let (listener, endpoint) = listener();
    let server = std::thread::spawn(move || {
        let mut stream = accept(&listener);
        let request = request(&mut stream);
        assert!(request.contains("%2Fsuite/"));
        serve_recipe(
            &mut stream,
            "pkgbase = suite\n\tpkgver = 2.0\n\tpkgrel = 1\n\tsource = https://example.org/suite-2.0.tar.gz\npkgname = suite-cli\npkgname = suite-docs\n",
        );
    });
    let output = command(data.path())
        .args([
            "refresh",
            "--recipes-only",
            "--dependency",
            "--arch-api-url",
            &endpoint,
        ])
        .env("PATH", bin.path())
        .output()
        .unwrap();
    assert!(output.status.success());
    server.join().unwrap();
    let database = Connection::open(data.path().join("paclang/paclang.db")).unwrap();
    assert_eq!(
        database
            .query_row("SELECT count(*) FROM provider_attempts", [], |row| row
                .get::<_, u32>(0))
            .unwrap(),
        2
    );
    assert_eq!(
        database
            .query_row("SELECT count(*) FROM installed_packages", [], |row| row
                .get::<_, u32>(0))
            .unwrap(),
        4
    );
}

#[test]
fn inventory_changes_during_recipe_fetch_mark_the_published_snapshot_stale() {
    let data = tempfile::tempdir().unwrap();
    let bin = fixture_bin();
    let (listener, endpoint) = listener();
    let pacman = bin.path().join("pacman");
    let server = std::thread::spawn(move || {
        let mut stream = accept(&listener);
        request(&mut stream);
        // Initial capture is already complete. The final sample sees an upgrade.
        super::write_executable(
            &pacman,
            "#!/bin/sh\nprintf 'alpha 9.0-1\\nforeign-tool 3.0-1\\nsuite-cli 2.0-1\\nsuite-docs 2.0-1\\n'\n",
        );
        serve_recipe(&mut stream, "invalid .SRCINFO");
    });
    let output = command(data.path())
        .args([
            "refresh",
            "--recipes-only",
            "--dependency",
            "--arch-api-url",
            &endpoint,
            "--format",
            "json",
        ])
        .env("PATH", bin.path())
        .output()
        .unwrap();
    server.join().unwrap();
    assert!(output.status.success());
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["snapshot_staleness"], "stale");
    let database = Connection::open(data.path().join("paclang/paclang.db")).unwrap();
    assert_eq!(
        database
            .query_row(
                "SELECT version FROM installed_packages WHERE name = 'alpha'",
                [],
                |row| row.get::<_, String>(0)
            )
            .unwrap(),
        "1.0-1"
    );
}
