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

fn full_refresh(data: &std::path::Path, bin: &std::path::Path, endpoint: &str) {
    let output = command(data)
        .args(["refresh", "--recipes-only", "--arch-api-url", endpoint])
        .env("PATH", bin)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
}

fn serve_initial_recipes(listener: TcpListener) {
    for _ in 0..3 {
        let mut stream = accept(&listener);
        let request = request(&mut stream);
        if request.contains("%2Falpha/") {
            serve_recipe(
                &mut stream,
                "pkgbase = alpha\n\tpkgver = 1.0\n\tpkgrel = 1\n\tsource = git+https://example.org/alpha.git#tag=v1.0\npkgname = alpha\n",
            );
        } else if request.contains("%2Fsuite/") {
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
    }
}

fn resolved_recipe_commits(data: &std::path::Path) -> Vec<(String, String)> {
    let database = Connection::open(data.join("paclang/paclang.db")).unwrap();
    let mut statement = database
        .prepare("SELECT package_name, recipe_commit FROM provider_attempts WHERE outcome = 'resolved' ORDER BY package_name")
        .unwrap();
    statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
        .into_iter()
        .map(|(name, commit)| (name, commit.unwrap()))
        .collect()
}

#[test]
fn resolved_attempts_are_reused_without_refetching() {
    let data = tempfile::tempdir().unwrap();
    let bin = fixture_bin();
    let (listener, endpoint) = listener();
    let server = std::thread::spawn(move || serve_initial_recipes(listener));
    full_refresh(data.path(), bin.path(), &endpoint);
    server.join().unwrap();
    assert_eq!(snapshot_count(data.path()), 1);
    // Dependency-only selects suite-cli and suite-docs, both already
    // resolved. A dead endpoint proves no fetch is attempted: any request
    // would fail and flip the rows to unresolved.
    let dead = TcpListener::bind("127.0.0.1:0").unwrap();
    let dead_endpoint = format!("http://{}/api/v4", dead.local_addr().unwrap());
    drop(dead);
    let output = command(data.path())
        .args([
            "refresh",
            "--recipes-only",
            "--dependency",
            "--arch-api-url",
            &dead_endpoint,
        ])
        .env("PATH", bin.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert_eq!(snapshot_count(data.path()), 2);
    assert_eq!(
        resolved_recipe_commits(data.path()),
        [
            ("alpha".to_owned(), COMMIT.to_owned()),
            ("suite-cli".to_owned(), COMMIT.to_owned()),
            ("suite-docs".to_owned(), COMMIT.to_owned()),
        ]
    );
}

#[test]
fn unresolved_attempts_are_retried_while_resolved_are_reused() {
    let data = tempfile::tempdir().unwrap();
    let bin = fixture_bin();
    let (initial_listener, endpoint) = listener();
    let server = std::thread::spawn(move || serve_initial_recipes(initial_listener));
    full_refresh(data.path(), bin.path(), &endpoint);
    server.join().unwrap();
    // Only foreign-tool is unresolved, so the next full refresh must fetch
    // exactly its group and leave the resolved rows untouched.
    let (retry_listener, endpoint) = listener();
    let server = std::thread::spawn(move || {
        let mut stream = accept(&retry_listener);
        let request = request(&mut stream);
        assert!(request.contains("%2Fforeign-tool/"), "{request}");
        stream
            .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .unwrap();
    });
    full_refresh(data.path(), bin.path(), &endpoint);
    server.join().unwrap();
    assert_eq!(
        resolved_recipe_commits(data.path()),
        [
            ("alpha".to_owned(), COMMIT.to_owned()),
            ("suite-cli".to_owned(), COMMIT.to_owned()),
            ("suite-docs".to_owned(), COMMIT.to_owned()),
        ]
    );
    let database = Connection::open(data.path().join("paclang/paclang.db")).unwrap();
    assert_eq!(
        database
            .query_row(
                "SELECT failure_reason FROM provider_attempts WHERE package_name = 'foreign-tool'",
                [],
                |row| row.get::<_, Option<String>>(0)
            )
            .unwrap()
            .as_deref(),
        Some("recipe API returned HTTP 404")
    );
}

#[test]
fn piped_stderr_stays_silent_during_recipe_refresh() {
    let data = tempfile::tempdir().unwrap();
    let bin = fixture_bin();
    let (listener, endpoint) = listener();
    let server = std::thread::spawn(move || serve_initial_recipes(listener));
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
    assert!(output.stderr.is_empty(), "progress leaked to piped stderr");
    server.join().unwrap();
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
        vec!["refresh", "--metadata-only", "--cachyos"],
        vec![
            "refresh",
            "--recipes-only",
            "--cachyos-api-url",
            "https://example.org",
        ],
        vec![
            "refresh",
            "--recipes-only",
            "--cachyos",
            "--cachyos-api-url",
            "http://example.org",
        ],
        vec!["refresh", "--metadata-only", "--recipes-only"],
        vec![
            "refresh",
            "--metadata-only",
            "--arch-api-url",
            "https://example.org",
        ],
        vec![
            "refresh",
            "--metadata-only",
            "--proxy-url",
            "https://proxy.example.org:8080",
        ],
        vec![
            "refresh",
            "--recipes-only",
            "--proxy-url",
            "http://proxy.example.org:8080",
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

#[test]
fn provider_report_exposes_stored_evidence_in_every_format() {
    let data = tempfile::tempdir().unwrap();
    let bin = fixture_bin();
    let (server, endpoint) = listener();
    let worker = std::thread::spawn(move || serve_initial_recipes(server));
    full_refresh(data.path(), bin.path(), &endpoint);
    worker.join().unwrap();
    let db = Connection::open(data.path().join("paclang/paclang.db")).unwrap();
    // An older version must not leak into the current package's report.
    db.execute(
        "UPDATE provider_attempts SET package_version = 'old' WHERE package_name = 'suite-docs'",
        [],
    )
    .unwrap();
    // Preserve failures from other providers even when a candidate resolved.
    db.execute("INSERT INTO provider_attempts (package_name, package_version, provider, outcome, failure_reason, recorded_at, recipe_location, recipe_commit) VALUES ('alpha', '1.0-1', 'other', 'unresolved', 'patch | <unknown> reason', 'now', 'https://example.org/recipe', ?1)", [COMMIT]).unwrap();
    let inspect = |format: &str, extra: &[&str]| {
        command(data.path())
            .args(["providers", "--format", format])
            .args(extra)
            .env("PATH", bin.path())
            .output()
            .unwrap()
    };
    let output = inspect("json", &[]);
    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["report"], "providers");
    assert_eq!(json["snapshot_staleness"], "current");
    assert_eq!(json["selected_population"], 4);
    assert_eq!(json["resolved"], 2);
    assert_eq!(json["unresolved"], 1);
    assert_eq!(json["not_attempted"], 1);
    let alpha = &json["packages"][0];
    assert_eq!(alpha["name"], "alpha");
    assert_eq!(alpha["outcome"], "resolved");
    assert_eq!(alpha["attempts"][0]["grade"], "reconstructed");
    assert_eq!(alpha["attempts"][0]["target"]["revision"], "tag:v1.0");
    assert_eq!(alpha["attempts"][0]["recipe"]["commit"], COMMIT);
    assert_eq!(alpha["attempts"][1]["outcome"], "unresolved");
    assert_eq!(alpha["attempts"][1]["recipe"]["commit"], COMMIT);
    assert!(json["packages"][1]["attempts"][0]["recipe"].is_null());
    assert_eq!(json["packages"][3]["outcome"], "not_attempted");
    assert_eq!(json["packages"][3]["attempts"], serde_json::json!([]));
    let output = inspect("toon", &[]);
    assert!(output.status.success());
    let mut toon: serde_json::Value =
        toon_format::decode_default(std::str::from_utf8(&output.stdout).unwrap()).unwrap();
    assert_eq!(
        toon.as_object_mut()
            .unwrap()
            .remove("toon_specification_version")
            .unwrap(),
        "3.0"
    );
    assert_eq!(toon, json);
    for format in ["terminal", "markdown"] {
        let output = inspect(format, &[]);
        assert!(output.status.success());
        let text = String::from_utf8(output.stdout).unwrap();
        for expected in [
            "Resolved: 2",
            "Unresolved: 1",
            "Not attempted: 1",
            "reconstructed",
            COMMIT,
            "tag:v1.0",
            "Recipe: unavailable",
        ] {
            assert!(text.contains(expected), "{format}: {text}");
        }
        assert!(!text.contains("<unknown>\nreason"));
    }
    let output = inspect("json", &["--explicit", "--origin", "unknown"]);
    let selected: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(selected["selected_population"], 2);
    assert_eq!(selected["installed_total"], 4);
    let output = inspect("json", &["--origin", "absent", "--min-coverage", "100"]);
    assert!(output.status.success());
    let empty: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(empty["packages"], serde_json::json!([]));
    assert_eq!(empty["selected_population"], 0);
    assert_eq!(
        inspect("json", &["--min-coverage", "1"]).status.code(),
        Some(1)
    );
    db.execute(
        "UPDATE installed_packages SET version = 'new' WHERE name = 'alpha'",
        [],
    )
    .unwrap();
    let output = inspect("json", &[]);
    let changed: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(changed["packages"][0]["outcome"], "not_attempted");
}

#[test]
fn provider_report_handles_missing_snapshot_and_unattempted_inventory() {
    let data = tempfile::tempdir().unwrap();
    let bin = fixture_bin();
    let inspect = || {
        command(data.path())
            .args(["providers", "--format", "json"])
            .env("PATH", bin.path())
            .output()
            .unwrap()
    };
    let output = inspect();
    assert!(!output.status.success());
    let error: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(error["error"]["code"], "no_snapshot");
    assert!(refresh(&[], data.path(), bin.path()).status.success());
    let output = inspect();
    assert!(output.status.success());
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["not_attempted"], 4);
    assert_eq!(report["resolved"], 0);
    assert_eq!(report["unresolved"], 0);
    super::write_executable(
        &bin.path().join("pacman"),
        "#!/bin/sh\nprintf 'alpha 9.0-1\\n'\n",
    );
    let output = inspect();
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["snapshot_staleness"], "stale");
}

#[test]
fn cachyos_recipes_and_arch_rebuild_fallbacks_are_acquired() {
    use sha2::{Digest, Sha256};
    const BLOB: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    let data = tempfile::tempdir().unwrap();
    let bin = fixture_bin();
    // The split package has a CachyOS rebuild suffix; its upstream recipe keeps -1.
    for name in ["pacman", "expac"] {
        let path = bin.path().join(name);
        let text = std::fs::read_to_string(&path)
            .unwrap()
            .replace("2.0-1", "2.0-1.1");
        std::fs::write(path, text).unwrap();
    }
    let (listener, endpoint) = listener();
    let archive_url = endpoint.clone();
    let thread = std::thread::spawn(move || {
        let archive = super::sources::tar();
        let checksum = format!("{:x}", Sha256::digest(&archive));
        let mut routes = Vec::new();
        for _ in 0..9 {
            let mut stream = accept(&listener);
            let request = request(&mut stream);
            let route = request.split_whitespace().nth(1).unwrap().to_owned();
            routes.push(route.clone());
            let recipe = |base: &str, names: &str, version: &str| {
                format!(
                    "pkgbase = {base}\n\tpkgver = {version}\n\tpkgrel = 1\n\tsource = {archive_url}/{base}-{version}.tar\n\tsha256sums = {checksum}\n{names}\n"
                )
            };
            let body = if route == "/api/v4/commits/master" {
                serde_json::to_vec(&serde_json::json!({"sha": COMMIT})).unwrap()
            } else if route == format!("/api/v4/git/trees/{COMMIT}?recursive=1") {
                serde_json::to_vec(&serde_json::json!({"truncated":false,"tree":[{"path":"alpha/.SRCINFO","type":"blob","sha":BLOB}]})).unwrap()
            } else if route == format!("/api/v4/git/blobs/{BLOB}") {
                serde_json::to_vec(&serde_json::json!({"sha":BLOB,"encoding":"base64","content":base64::engine::general_purpose::STANDARD.encode(recipe("alpha", "pkgname = alpha", "1.0"))})).unwrap()
            } else if route.contains("%2Fsuite/") && route.ends_with("?ref=2.0-1") {
                serve_recipe(
                    &mut stream,
                    &recipe("suite", "pkgname = suite-cli\npkgname = suite-docs", "2.0"),
                );
                continue;
            } else if route.ends_with(".tar") {
                archive.clone()
            } else {
                assert!(route.contains("/projects/"), "unexpected request: {route}");
                stream
                    .write_all(
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    )
                    .unwrap();
                continue;
            };
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .unwrap();
            stream.write_all(&body).unwrap();
        }
        assert_eq!(
            routes
                .iter()
                .filter(|route| route.contains("/git/blobs/"))
                .count(),
            1
        );
        assert_eq!(
            routes
                .iter()
                .filter(|route| route.contains("%2Fsuite/") && route.ends_with("?ref=2.0-1"))
                .count(),
            1
        );
    });
    let output = command(data.path())
        .args([
            "refresh",
            "--sources-only",
            "--cachyos",
            "--arch-api-url",
            &endpoint,
            "--cachyos-api-url",
            &endpoint,
            "--format",
            "json",
        ])
        .env("PATH", bin.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    thread.join().unwrap();
    let db = Connection::open(data.path().join("paclang/paclang.db")).unwrap();
    let rows: Vec<(String,String,String,String)> = db.prepare("SELECT package_name, outcome, coalesce(evidence_grade,''), coalesce(recipe_commit,'') FROM provider_attempts WHERE provider='cachyos' ORDER BY package_name").unwrap().query_map([], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?))).unwrap().map(Result::unwrap).collect();
    assert_eq!(
        rows,
        vec![
            (
                "alpha".into(),
                "resolved".into(),
                "reconstructed".into(),
                COMMIT.into()
            ),
            (
                "foreign-tool".into(),
                "unresolved".into(),
                "".into(),
                "".into()
            ),
            (
                "suite-cli".into(),
                "resolved".into(),
                "fallback".into(),
                COMMIT.into()
            ),
            (
                "suite-docs".into(),
                "resolved".into(),
                "fallback".into(),
                COMMIT.into()
            ),
        ]
    );
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM source_acquisitions WHERE state='acquired'",
            [],
            |row| row.get::<_, i64>(0)
        )
        .unwrap(),
        2
    );
    let provenance: String = db.query_row("SELECT recipe_location FROM provider_attempts WHERE provider='cachyos' AND package_name='alpha'", [], |row| row.get(0)).unwrap();
    assert!(provenance.ends_with(&format!("/contents/alpha/.SRCINFO?ref={COMMIT}")));
}

#[test]
fn interrupting_cachyos_metadata_rolls_back_arch_attempts() {
    let data = tempfile::tempdir().unwrap();
    let bin = fixture_bin();
    assert!(refresh(&[], data.path(), bin.path()).status.success());
    let (listener, endpoint) = listener();
    let (started_tx, started_rx) = mpsc::channel();
    let (finish_tx, finish_rx) = mpsc::channel();
    let server = std::thread::spawn(move || {
        for _ in 0..3 {
            let mut stream = accept(&listener);
            assert!(request(&mut stream).contains("/projects/"));
            stream
                .write_all(
                    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .unwrap();
        }
        let mut stream = accept(&listener);
        assert!(request(&mut stream).contains("/commits/master"));
        started_tx.send(()).unwrap();
        finish_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    });
    let child = command(data.path())
        .args([
            "refresh",
            "--recipes-only",
            "--cachyos",
            "--arch-api-url",
            &endpoint,
            "--cachyos-api-url",
            &endpoint,
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
    let db = Connection::open(data.path().join("paclang/paclang.db")).unwrap();
    assert_eq!(
        db.query_row("SELECT count(*) FROM provider_attempts", [], |row| row
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
}

#[test]
fn metapackages_need_no_acquisition_or_cachyos_fallback_and_are_cached() {
    let data = tempfile::tempdir().unwrap();
    let bin = fixture_bin();
    let (listener, endpoint) = listener();
    let thread = std::thread::spawn(move || {
        for _ in 0..3 {
            let mut stream = accept(&listener);
            let route = request(&mut stream);
            let recipe = if route.contains("%2Falpha/") {
                "pkgbase = alpha\n\tpkgver = 1.0\n\tpkgrel = 1\n\tdepends = glibc\npkgname = alpha\n"
            } else if route.contains("%2Fsuite/") {
                "pkgbase = suite\n\tpkgver = 2.0\n\tpkgrel = 1\n\tdepends = glibc\npkgname = suite-cli\npkgname = suite-docs\n"
            } else {
                assert!(route.contains("%2Fforeign-tool/"));
                "pkgbase = foreign-tool\n\tpkgver = 3.0\n\tpkgrel = 1\n\tdepends = glibc\npkgname = foreign-tool\n"
            };
            serve_recipe(&mut stream, recipe);
        }
    });
    for _ in 0..2 {
        let output = command(data.path())
            .args([
                "refresh",
                "--sources-only",
                "--cachyos",
                "--require-complete",
                "--min-coverage",
                "100",
                "--arch-api-url",
                &endpoint,
                "--cachyos-api-url",
                &endpoint,
                "--format",
                "json",
            ])
            .env("PATH", bin.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["coverage"]["denominator"], 0);
        assert_eq!(value["coverage"]["source_free_packages"], 4);
        assert_eq!(value["coverage"]["missing_source_language_analysis"], 0);
        assert_eq!(value["source_acquisition"]["targets"], 0);
        assert_eq!(value["source_acquisition"]["unresolved_packages"], 0);
        assert_eq!(value["source_acquisition"]["source_free_packages"], 4);
    }
    thread.join().unwrap();
    let output = command(data.path())
        .args(["providers", "--format", "json"])
        .env("PATH", bin.path())
        .output()
        .unwrap();
    assert!(output.status.success());
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["source_free"], 4);
    assert_eq!(value["unresolved"], 0);
    for package in value["packages"].as_array().unwrap() {
        assert_eq!(package["outcome"], "source_free");
        assert_eq!(package["attempts"].as_array().unwrap().len(), 1);
        assert!(package["attempts"][0]["target"].is_null());
        assert!(package["attempts"][0]["failure_reason"].is_null());
    }
    let output = command(data.path())
        .args(["providers", "--format", "terminal"])
        .env("PATH", bin.path())
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&output.stdout).contains("Acquisition: not required"));
    let output = command(data.path())
        .args([
            "count",
            "--explicit",
            "--min-coverage",
            "100",
            "--format",
            "json",
        ])
        .env("PATH", bin.path())
        .output()
        .unwrap();
    assert!(output.status.success());
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["selected_population"], 2);
    assert_eq!(value["coverage"]["denominator"], 0);
    assert_eq!(value["coverage"]["source_free_packages"], 2);
}
