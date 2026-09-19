use super::{command, fixture_bin, refresh, write_executable};
use rusqlite::Connection;
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::time::Duration;

pub(super) fn tar() -> Vec<u8> {
    let content = b"fn main() {}\n";
    let mut header = [0u8; 512];
    header[..13].copy_from_slice(b"source/lib.rs");
    header[100..108].copy_from_slice(b"0000644\0");
    header[124..136].copy_from_slice(format!("{:011o}\0", content.len()).as_bytes());
    header[148..156].fill(b' ');
    header[156] = b'0';
    header[257..263].copy_from_slice(b"ustar\0");
    header[263..265].copy_from_slice(b"00");
    let sum: u32 = header.iter().map(|b| u32::from(*b)).sum();
    header[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
    let mut bytes = header.to_vec();
    bytes.extend_from_slice(content);
    bytes.resize(2048, 0);
    bytes
}

struct Server {
    url: String,
    stop: Arc<AtomicBool>,
    hits: Arc<AtomicUsize>,
    maximum: Arc<AtomicUsize>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Server {
    fn new(body: Vec<u8>, delay: Duration) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let stop = Arc::new(AtomicBool::new(false));
        let hits = Arc::new(AtomicUsize::new(0));
        let maximum = Arc::new(AtomicUsize::new(0));
        let (done, count, max) = (stop.clone(), hits.clone(), maximum.clone());
        let thread = std::thread::spawn(move || {
            let active = Arc::new(AtomicUsize::new(0));
            let mut workers = Vec::new();
            while !done.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let (body, count, max, active) =
                            (body.clone(), count.clone(), max.clone(), active.clone());
                        workers.push(std::thread::spawn(move || {
                            stream.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
                            let mut request = Vec::new();
                            while !request.ends_with(b"\r\n\r\n") { let mut byte = [0]; if stream.read_exact(&mut byte).is_err() { return; } request.push(byte[0]); }
                            count.fetch_add(1, Ordering::SeqCst);
                            let current = active.fetch_add(1, Ordering::SeqCst) + 1; max.fetch_max(current, Ordering::SeqCst);
                            std::thread::sleep(delay);
                            let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                            let _ = stream.write_all(&body); active.fetch_sub(1, Ordering::SeqCst);
                        }));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5))
                    }
                    Err(error) => panic!("{error}"),
                }
            }
            for worker in workers {
                worker.join().unwrap();
            }
        });
        Self {
            url,
            stop,
            hits,
            maximum,
            thread: Some(thread),
        }
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.thread.take().unwrap().join().unwrap();
    }
}

fn seed(data: &Path, bin: &Path, url: &str, checksum: &str, separate: bool) {
    assert!(refresh(&[], data, bin).status.success());
    let db = Connection::open(data.join("paclang/paclang.db")).unwrap();
    for (index, (name, version)) in [
        ("alpha", "1.0-1"),
        ("foreign-tool", "3.0-1"),
        ("suite-cli", "2.0-1"),
        ("suite-docs", "2.0-1"),
    ]
    .iter()
    .enumerate()
    {
        let location = format!("{url}/{}.tar", if separate { index } else { 0 });
        db.execute("INSERT OR IGNORE INTO analysis_targets (location, revision, subdirectory) VALUES (?1, '', '')", [&location]).unwrap();
        db.execute("INSERT INTO provider_attempts (package_name, package_version, provider, outcome, target_id, evidence_grade, recorded_at, source_checksum) SELECT ?1, ?2, 'arch', 'resolved', id, 'reconstructed', 'old', ?4 FROM analysis_targets WHERE location = ?3", rusqlite::params![name, version, location, checksum]).unwrap();
    }
}

#[test]
fn sources_refresh_verifies_deduplicates_reports_and_cleans_up() {
    let data = tempfile::tempdir().unwrap();
    let bin = fixture_bin();
    let bytes = tar();
    let checksum = format!("sha256:{:x}", Sha256::digest(&bytes));
    let server = Server::new(bytes, Duration::ZERO);
    seed(data.path(), bin.path(), &server.url, &checksum, false);
    let output = command(data.path())
        .args(["refresh", "--sources-only", "--format", "json"])
        .env("PATH", bin.path())
        .env("XDG_CACHE_HOME", data.path().join("cache"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let refresh_report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(refresh_report["source_acquisition"]["targets"], 1);
    assert_eq!(refresh_report["source_acquisition"]["acquired"], 1);
    assert_eq!(refresh_report["coverage"]["analyzed"], 0);
    assert_eq!(server.hits.load(Ordering::SeqCst), 1);
    assert_eq!(
        std::fs::read_dir(data.path().join("cache/paclang/sources"))
            .unwrap()
            .count(),
        0
    );
    let output = command(data.path())
        .args(["providers", "--format", "json"])
        .env("PATH", bin.path())
        .output()
        .unwrap();
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    for package in report["packages"].as_array().unwrap() {
        let result = &package["attempts"][0]["acquisition"];
        assert_eq!(result["state"], "acquired");
        assert_eq!(result["downloaded_bytes"], 2048);
        assert_eq!(result["extracted_bytes"], 13);
        assert_eq!(
            result["content_sha256"],
            checksum.strip_prefix("sha256:").unwrap()
        );
    }
}

#[test]
fn sources_refresh_runs_linguist_and_reports_coverage() {
    let data = tempfile::tempdir().unwrap();
    let bin = fixture_bin();
    let bytes = tar();
    let checksum = format!("sha256:{:x}", Sha256::digest(&bytes));
    let server = Server::new(bytes, Duration::ZERO);
    seed(data.path(), bin.path(), &server.url, &checksum, false);
    let linguist = bin.path().join("github-linguist");
    write_executable(
        &linguist,
        "#!/bin/sh\nprintf '{\"Rust\": {\"size\": 100}, \"C\": {\"size\": 25}}'\n",
    );
    let output = command(data.path())
        .args(["refresh", "--sources-only", "--format", "json"])
        .arg(format!(
            "--linguist-executable={}",
            linguist.to_string_lossy()
        ))
        .env("PATH", bin.path())
        .env("XDG_CACHE_HOME", data.path().join("cache"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let refresh_report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    // All four fixture packages share one analysis target.
    assert_eq!(refresh_report["coverage"]["analyzed"], 4);
    assert_eq!(
        refresh_report["coverage"]["missing_source_language_analysis"],
        0
    );
    let db = Connection::open(data.path().join("paclang/paclang.db")).unwrap();
    let profile: String = db
        .query_row("SELECT state FROM language_profiles", [], |row| row.get(0))
        .unwrap();
    assert_eq!(profile, "analyzed");
    assert_eq!(
        db.query_row(
            "SELECT primary_language FROM language_profiles",
            [],
            |row| row.get::<_, Option<String>>(0)
        )
        .unwrap()
        .as_deref(),
        Some("Rust")
    );
    let output = command(data.path())
        .args(["count", "--format", "json"])
        .env("PATH", bin.path())
        .output()
        .unwrap();
    let count: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(count["coverage"]["analyzed"], 4);
    let output = command(data.path())
        .args(["providers", "--format", "json"])
        .env("PATH", bin.path())
        .output()
        .unwrap();
    let providers: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    for package in providers["packages"].as_array().unwrap() {
        let analysis = &package["attempts"][0]["analysis"];
        assert_eq!(analysis["state"], "analyzed");
        assert_eq!(analysis["primary_language"], "Rust");
    }
    // Coverage gates now see the analyzed share.
    let output = command(data.path())
        .args(["count", "--format", "json", "--min-coverage", "100"])
        .env("PATH", bin.path())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0));
}

#[test]
fn sources_limits_and_checksum_mismatch_are_partial_data() {
    for (args, bad_checksum, state) in [
        (
            vec!["--max-download-bytes", "100"],
            false,
            "resource_limited",
        ),
        (
            vec!["--max-extracted-bytes", "2"],
            false,
            "resource_limited",
        ),
        (vec!["--max-source-files", "1"], false, "resource_limited"),
        (vec![], true, "failed"),
    ] {
        let data = tempfile::tempdir().unwrap();
        let bin = fixture_bin();
        let bytes = tar();
        let checksum = format!(
            "sha256:{:x}",
            Sha256::digest(if bad_checksum {
                b"wrong".as_slice()
            } else {
                &bytes
            })
        );
        let server = Server::new(bytes, Duration::ZERO);
        seed(data.path(), bin.path(), &server.url, &checksum, false);
        let output = command(data.path())
            .args(["refresh", "--sources-only"])
            .args(args)
            .env("PATH", bin.path())
            .env("XDG_CACHE_HOME", data.path().join("cache"))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
        let db = Connection::open(data.path().join("paclang/paclang.db")).unwrap();
        assert_eq!(
            db.query_row("SELECT state FROM source_acquisitions", [], |row| row
                .get::<_, String>(0))
                .unwrap(),
            state
        );
        if bad_checksum {
            let output = command(data.path())
                .args(["refresh", "--sources-only", "--require-complete"])
                .env("PATH", bin.path())
                .env("XDG_CACHE_HOME", data.path().join("cache"))
                .output()
                .unwrap();
            assert_eq!(output.status.code(), Some(1));
            assert_eq!(
                db.query_row("SELECT count(*) FROM inventory_snapshots", [], |row| row
                    .get::<_, i64>(0))
                    .unwrap(),
                3
            );
        }
        assert_eq!(
            std::fs::read_dir(data.path().join("cache/paclang/sources"))
                .unwrap()
                .count(),
            0
        );
    }
}

#[test]
fn sources_worker_deadline_and_concurrency_are_enforced() {
    let data = tempfile::tempdir().unwrap();
    let bin = fixture_bin();
    let bytes = tar();
    let checksum = format!("sha256:{:x}", Sha256::digest(&bytes));
    let server = Server::new(bytes, Duration::from_millis(1200));
    seed(data.path(), bin.path(), &server.url, &checksum, true);
    let output = command(data.path())
        .args([
            "refresh",
            "--sources-only",
            "--source-jobs",
            "2",
            "--source-timeout-seconds",
            "1",
        ])
        .env("PATH", bin.path())
        .env("XDG_CACHE_HOME", data.path().join("cache"))
        .output()
        .unwrap();
    assert!(output.status.success());
    let db = Connection::open(data.path().join("paclang/paclang.db")).unwrap();
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM source_acquisitions WHERE state = 'resource_limited'",
            [],
            |row| row.get::<_, i64>(0)
        )
        .unwrap(),
        4
    );
    // Requests can remain at the server after client cancellation; the worker
    // cap applies to live acquisition processes, not server-side sleep tasks.
    assert!(server.maximum.load(Ordering::SeqCst) >= 2);
    assert_eq!(
        std::fs::read_dir(data.path().join("cache/paclang/sources"))
            .unwrap()
            .count(),
        0
    );
}

#[test]
fn source_interruption_rolls_back_and_reaps_workers() {
    let data = tempfile::tempdir().unwrap();
    let bin = fixture_bin();
    let bytes = tar();
    let checksum = format!("sha256:{:x}", Sha256::digest(&bytes));
    let server = Server::new(bytes, Duration::from_secs(2));
    seed(data.path(), bin.path(), &server.url, &checksum, false);
    let child = command(data.path())
        .args(["refresh", "--sources-only"])
        .env("PATH", bin.path())
        .env("XDG_CACHE_HOME", data.path().join("cache"))
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while server.hits.load(Ordering::SeqCst) == 0 {
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(child.id() as i32),
        nix::sys::signal::Signal::SIGINT,
    )
    .unwrap();
    assert_eq!(child.wait_with_output().unwrap().status.code(), Some(130));
    let db = Connection::open(data.path().join("paclang/paclang.db")).unwrap();
    assert_eq!(
        db.query_row("SELECT count(*) FROM source_acquisitions", [], |row| row
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert_eq!(
        db.query_row("SELECT count(*) FROM inventory_snapshots", [], |row| row
            .get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        std::fs::read_dir(data.path().join("cache/paclang/sources"))
            .unwrap()
            .count(),
        0
    );
}

#[test]
fn git_worker_fetches_only_pinned_revisions_and_materializes_a_clean_repo() {
    let upstream = tempfile::tempdir().unwrap();
    let signature = git2::Signature::now("Fixture", "fixture@example.org").unwrap();
    let mut child_pin = None;
    let mut pins = Vec::new();
    for name in ["grandchild.git", "child.git"] {
        let child = git2::Repository::init_bare(upstream.path().join(name)).unwrap();
        child.config().unwrap().set_bool("uploadpack.allowReachableSHA1InWant", true).unwrap();
        let mut files = child.treebuilder(None).unwrap();
        files.insert("pinned.rs", child.blob(b"pinned child source\n").unwrap(), 0o100644).unwrap();
        if let Some(pin) = child_pin {
            files.insert("nested", pin, 0o160000).unwrap();
            files.insert(".gitmodules", child.blob(b"[submodule \"nested\"]\npath = nested\nurl = ../grandchild.git\nupdate = !touch /tmp/paclang-must-not-run\n").unwrap(), 0o100644).unwrap();
        }
        let tree = child.find_tree(files.write().unwrap()).unwrap();
        let pin = child.commit(Some("refs/heads/main"), &signature, &signature, "pinned", &tree, &[]).unwrap();
        files.insert("pinned.rs", child.blob(b"wrong branch HEAD\n").unwrap(), 0o100644).unwrap();
        child.commit(Some("refs/heads/main"), &signature, &signature, "new HEAD", &child.find_tree(files.write().unwrap()).unwrap(), &[&child.find_commit(pin).unwrap()]).unwrap();
        child.set_head("refs/heads/main").unwrap();
        pins.push(pin);
        child_pin = Some(pin);
    }
    let repo = git2::Repository::init_bare(upstream.path().join("upstream.git")).unwrap();
    repo.config()
        .unwrap()
        .set_bool("uploadpack.allowReachableSHA1InWant", true)
        .unwrap();
    let blob = repo.blob(b"packaged source\n").unwrap();
    let mut builder = repo.treebuilder(None).unwrap();
    builder.insert("main.rs", blob, 0o100644).unwrap();
    builder.insert("vendor", child_pin.unwrap(), 0o160000).unwrap();
    builder.insert(".gitmodules", repo.blob(b"[submodule \"vendor\"]\npath = vendor\nurl = ../child.git\nbranch = main\nupdate = none\n").unwrap(), 0o100644).unwrap();
    let link = repo.blob(b".").unwrap();
    builder.insert("self", link, 0o120000).unwrap();
    let tree = repo.find_tree(builder.write().unwrap()).unwrap();
    let signature = git2::Signature::now("Fixture", "fixture@example.org").unwrap();
    let pinned = repo
        .commit(
            Some("refs/heads/main"),
            &signature,
            &signature,
            "packaged",
            &tree,
            &[],
        )
        .unwrap();
    repo.tag_lightweight("v1.0", &repo.find_object(pinned, None).unwrap(), false)
        .unwrap();
    let current = repo
        .blob(b"current upstream must not replace packaged source\n")
        .unwrap();
    builder.insert("main.rs", current, 0o100644).unwrap();
    repo.commit(
        Some("refs/heads/main"),
        &signature,
        &signature,
        "newer",
        &repo.find_tree(builder.write().unwrap()).unwrap(),
        &[&repo.find_commit(pinned).unwrap()],
    )
    .unwrap();
    repo.set_head("refs/heads/main").unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}/upstream.git", listener.local_addr().unwrap());
    let stop = Arc::new(AtomicBool::new(false));
    let done = stop.clone();
    let root = upstream.path().to_path_buf();
    let server = std::thread::spawn(move || {
        while !done.load(Ordering::SeqCst) {
            let (mut stream, _) = match listener.accept() {
                Ok(stream) => stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                }
                Err(error) => panic!("{error}"),
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut headers = Vec::new();
            while !headers.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                stream.read_exact(&mut byte).unwrap();
                headers.push(byte[0]);
            }
            let headers = String::from_utf8(headers).unwrap();
            let mut first = headers.lines().next().unwrap().split_whitespace();
            let method = first.next().unwrap();
            let location = first.next().unwrap();
            let (path, query) = location.split_once('?').unwrap_or((location, ""));
            let length = headers
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length: ")
                        .map(|v| v.parse::<usize>().unwrap())
                })
                .unwrap_or(0);
            let mut body = vec![0; length];
            stream.read_exact(&mut body).unwrap();
            let mut cgi = std::process::Command::new("git")
                .arg("http-backend")
                .env("GIT_PROJECT_ROOT", &root)
                .env("GIT_HTTP_EXPORT_ALL", "1")
                .env("PATH_INFO", path)
                .env("QUERY_STRING", query)
                .env("REQUEST_METHOD", method)
                .env("CONTENT_TYPE", "application/x-git-upload-pack-request")
                .env("CONTENT_LENGTH", length.to_string())
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            cgi.stdin.take().unwrap().write_all(&body).unwrap();
            let output = cgi.wait_with_output().unwrap();
            assert!(output.status.success());
            let header_end = output
                .stdout
                .windows(4)
                .position(|window| window == b"\r\n\r\n")
                .unwrap()
                + 4;
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n",
                output.stdout.len() - header_end
            );
            let _ = stream.write_all(&output.stdout);
        }
    });
    let mut full_download = 0;
    for (revision, expected, download_limit, expanded_limit) in [
        (
            "tag:v1.0".to_owned(),
            "acquired",
            1_073_741_824,
            4_294_967_296u64,
        ),
        (
            format!("commit:{pinned}"),
            "acquired",
            1_073_741_824,
            4_294_967_296,
        ),
        (
            "tag:missing".to_owned(),
            "failed",
            1_073_741_824,
            4_294_967_296,
        ),
        ("tag:v1.0".to_owned(), "resource_limited", 1, 4_294_967_296),
        ("tag:v1.0".to_owned(), "resource_limited", 1_073_741_824, 1),
        ("aggregate-download".to_owned(), "resource_limited", 0, 4_294_967_296),
        ("aggregate-files".to_owned(), "resource_limited", 1_073_741_824, 4_294_967_296),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let selected_revision = if revision.starts_with("aggregate-") { "tag:v1.0" } else { &revision };
        let download_limit = if download_limit == 0 { full_download * 3 / 4 } else { download_limit };
        let file_limit = if revision == "aggregate-files" { 4 } else { 250000 };
        let job = serde_json::json!({"target": {"location": url, "revision": selected_revision, "subdirectory": null, "checksum": null}, "limits": {"source_jobs": 4, "max_download_bytes": download_limit, "max_extracted_bytes": expanded_limit, "max_source_files": file_limit, "source_timeout_seconds": 15}});
        std::fs::write(
            directory.path().join("job.json"),
            serde_json::to_vec(&job).unwrap(),
        )
        .unwrap();
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_paclang"))
            .arg("--acquisition-worker")
            .arg(directory.path())
            .env_clear()
            .env("HOME", directory.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let result: serde_json::Value =
            serde_json::from_slice(&std::fs::read(directory.path().join("result.json")).unwrap())
                .unwrap();
        assert_eq!(result["state"], expected, "{revision}: {result}");
        if revision == "aggregate-download" {
            assert_eq!(result["verified_revision"], pinned.to_string(), "budget should allow the parent: {result}");
            assert_eq!(result["reason"], "source download byte limit exceeded");
        }
        if expected == "acquired" {
            full_download = result["downloaded_bytes"].as_u64().unwrap();
            assert_eq!(result["verified_revision"], pinned.to_string());
            assert_eq!(result["submodules"].as_array().unwrap().len(), 2);
            assert_eq!(result["submodules"][0]["path"], "vendor");
            assert_eq!(result["submodules"][0]["verified_revision"], pins[1].to_string());
            assert_eq!(result["submodules"][1]["path"], "vendor/nested");
            assert_eq!(result["submodules"][1]["verified_revision"], pins[0].to_string());
            for path in ["vendor/pinned.rs", "vendor/nested/pinned.rs"] {
                assert_eq!(std::fs::read(directory.path().join("tree").join(path)).unwrap(), b"pinned child source\n");
            }
            assert!(!directory.path().join("tree/vendor/.git").exists());
            assert_eq!(
                std::fs::read(directory.path().join("tree/main.rs")).unwrap(),
                b"packaged source\n"
            );
            let materialized = git2::Repository::open(directory.path().join("tree")).unwrap();
            assert!(materialized.head().unwrap().peel_to_commit().is_ok());
            assert_eq!(materialized.remotes().unwrap().len(), 0);
        }
    }
    stop.store(true, Ordering::SeqCst);
    server.join().unwrap();
}

#[test]
fn source_concurrency_stays_within_the_configured_cap() {
    let data = tempfile::tempdir().unwrap();
    let bin = fixture_bin();
    let bytes = tar();
    let checksum = format!("sha512:{:x}", sha2::Sha512::digest(&bytes));
    let server = Server::new(bytes, Duration::from_millis(150));
    seed(data.path(), bin.path(), &server.url, &checksum, true);
    let output = command(data.path())
        .args(["refresh", "--sources-only", "--source-jobs", "2"])
        .env("PATH", bin.path())
        .env("XDG_CACHE_HOME", data.path().join("cache"))
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(server.hits.load(Ordering::SeqCst), 4);
    assert_eq!(server.maximum.load(Ordering::SeqCst), 2);
}

#[test]
fn archive_verification_accepts_b2_checksums() {
    let data = tempfile::tempdir().unwrap();
    let bin = fixture_bin();
    let bytes = tar();
    let checksum = format!("b2:{:x}", blake2::Blake2b512::digest(&bytes));
    let server = Server::new(bytes, Duration::ZERO);
    seed(data.path(), bin.path(), &server.url, &checksum, false);
    let output = command(data.path())
        .args(["refresh", "--sources-only", "--format", "json"])
        .env("PATH", bin.path())
        .env("XDG_CACHE_HOME", data.path().join("cache"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let refresh_report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(refresh_report["source_acquisition"]["targets"], 1);
    assert_eq!(refresh_report["source_acquisition"]["acquired"], 1);
}

#[test]
fn sources_refresh_upgrades_legacy_archive_evidence_before_downloading() {
    use base64::Engine;
    let data = tempfile::tempdir().unwrap();
    let bin = fixture_bin();
    let bytes = tar();
    let hash = format!("{:x}", Sha256::digest(&bytes));
    let server = Server::new(bytes, Duration::ZERO);
    seed(
        data.path(),
        bin.path(),
        &server.url,
        &format!("sha256:{hash}"),
        false,
    );
    let db = Connection::open(data.path().join("paclang/paclang.db")).unwrap();
    db.execute("UPDATE provider_attempts SET source_checksum = NULL", [])
        .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let endpoint = format!("http://{}/api/v4", listener.local_addr().unwrap());
    let source_url = server.url.clone();
    let recipe_server = std::thread::spawn(move || {
        for _ in 0..3 {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(std::time::Instant::now() < deadline);
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("{error}"),
                }
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                stream.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
            }
            let request = String::from_utf8(request).unwrap();
            let (base, version, members) = if request.contains("%2Falpha/") {
                ("alpha", "1.0", "pkgname = alpha\n")
            } else if request.contains("%2Fsuite/") {
                (
                    "suite",
                    "2.0",
                    "pkgname = suite-cli\npkgname = suite-docs\n",
                )
            } else {
                ("foreign-tool", "3.0", "pkgname = foreign-tool\n")
            };
            let metadata = format!(
                "pkgbase = {base}\n\tpkgver = {version}\n\tpkgrel = 1\n\tsource = {source_url}/{version}.tar\n\tsha256sums = {hash}\n{members}"
            );
            let response = serde_json::to_vec(&serde_json::json!({"file_path": ".SRCINFO", "encoding": "base64", "commit_id": "0123456789abcdef0123456789abcdef01234567", "content": base64::engine::general_purpose::STANDARD.encode(metadata)})).unwrap();
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                response.len()
            )
            .unwrap();
            stream.write_all(&response).unwrap();
        }
    });
    let output = command(data.path())
        .args([
            "refresh",
            "--sources-only",
            "--arch-api-url",
            &endpoint,
            "--require-complete",
        ])
        .env("PATH", bin.path())
        .env("XDG_CACHE_HOME", data.path().join("cache"))
        .output()
        .unwrap();
    recipe_server.join().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM provider_attempts WHERE source_checksum IS NOT NULL",
            [],
            |row| row.get::<_, i64>(0)
        )
        .unwrap(),
        4
    );
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM source_acquisitions WHERE state = 'acquired'",
            [],
            |row| row.get::<_, i64>(0)
        )
        .unwrap(),
        3
    );
    assert_eq!(server.hits.load(Ordering::SeqCst), 3);
}

#[test]
fn verified_prebuilt_archive_retains_binary_hashes_before_cleanup() {
    let data = tempfile::tempdir().unwrap();
    let bin = fixture_bin();
    let mut elf = vec![0u8; 64];
    elf[..7].copy_from_slice(b"\x7fELF\x02\x01\x01");
    elf[16] = 2;
    let mut builder = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_size(elf.len() as u64);
    header.set_mode(0o755);
    header.set_cksum();
    builder
        .append_data(&mut header, "prebuilt/app", elf.as_slice())
        .unwrap();
    let archive = builder.into_inner().unwrap();
    let checksum = format!("sha256:{:x}", Sha256::digest(&archive));
    let server = Server::new(archive, Duration::ZERO);
    seed(data.path(), bin.path(), &server.url, &checksum, false);
    let db = Connection::open(data.path().join("paclang/paclang.db")).unwrap();
    db.execute("UPDATE provider_attempts SET recipe_location='https://example.invalid/recipe', recipe_commit=?1", ["a".repeat(40)]).unwrap();
    let linguist = bin.path().join("github-linguist");
    write_executable(&linguist, "#!/bin/sh\nprintf '{}'\n");
    let output = command(data.path())
        .args([
            "refresh",
            "--sources-only",
            "--format",
            "json",
            "--linguist-executable",
        ])
        .arg(&linguist)
        .env("PATH", bin.path())
        .env("XDG_CACHE_HOME", data.path().join("cache"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let manifest: String = db
        .query_row(
            "SELECT manifest_json FROM source_payload_evidence",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let manifest: serde_json::Value = serde_json::from_str(&manifest).unwrap();
    assert_eq!(manifest["complete"], true);
    assert_eq!(
        manifest["binaries"][0]["sha256"],
        format!("{:x}", Sha256::digest(&elf))
    );
    assert_eq!(manifest["binaries"][0]["bytes"], 64);
    assert_eq!(
        db.query_row("SELECT count(*) FROM source_payload_evidence", [], |row| {
            row.get::<_, u64>(0)
        })
        .unwrap(),
        1
    );
    assert_eq!(
        std::fs::read_dir(data.path().join("cache/paclang/sources"))
            .unwrap()
            .count(),
        0
    );
    assert_eq!(server.hits.load(Ordering::SeqCst), 1);

    let installed = data.path().join("installed-app");
    std::fs::write(&installed, &elf).unwrap();
    let pacman = std::fs::read_to_string(bin.path().join("pacman")).unwrap();
    let wrapper = format!(
        "#!/bin/sh\nif [ \"$1\" = '-Qql' ]; then printf '%s\\n' '{}'; exit 0; fi\n",
        installed.display()
    );
    write_executable(
        &bin.path().join("pacman"),
        &pacman.replacen("#!/bin/sh\n", &wrapper, 1),
    );
    let output = command(data.path())
        .args(["blobs", "--package", "alpha", "--format", "json"])
        .env("PATH", bin.path())
        .output()
        .unwrap();
    assert!(output.status.success());
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["opaque_bytes"], 64);
    assert_eq!(report["unknown_bytes"], 56);
    assert_eq!(server.hits.load(Ordering::SeqCst), 1); // Reporting never downloads.
}

#[test]
fn git_worker_uses_explicit_proxy_for_https_connect() {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let proxy = format!("http://{}", listener.local_addr().unwrap());
    let server = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(std::time::Instant::now() < deadline, "Git bypassed explicit proxy");
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(e) => panic!("{e}"),
            }
        };
        stream.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
        let mut request = Vec::new();
        while !request.ends_with(b"\r\n\r\n") {
            let mut byte = [0];
            stream.read_exact(&mut byte).unwrap();
            request.push(byte[0]);
            assert!(request.len() < 8192);
        }
        assert!(String::from_utf8_lossy(&request).starts_with("CONNECT source.invalid:443 HTTP/"));
        stream.write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
    });
    let directory = tempfile::tempdir().unwrap();
    let job = serde_json::json!({
        "target": {"location": "https://source.invalid/repo.git", "revision": "tag:v1", "subdirectory": null, "checksum": null},
        "proxy": proxy,
        "limits": {"source_jobs": 1, "max_download_bytes": 1024, "max_extracted_bytes": 1024, "max_source_files": 100, "source_timeout_seconds": 10}
    });
    std::fs::write(directory.path().join("job.json"), serde_json::to_vec(&job).unwrap()).unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_paclang"))
        .arg("--acquisition-worker").arg(directory.path()).env_clear()
        .env("HOME", directory.path()).output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    server.join().unwrap();
    let result: serde_json::Value = serde_json::from_slice(&std::fs::read(directory.path().join("result.json")).unwrap()).unwrap();
    assert_eq!(result["state"], "failed");
}

#[test]
fn isolated_archive_worker_accepts_utf8_filenames() {
    let mut bytes = tar();
    let name = "source/日本語:тест.rs".as_bytes();
    bytes[..100].fill(0);
    bytes[..name.len()].copy_from_slice(name);
    bytes[148..156].fill(b' ');
    let sum: u32 = bytes[..512].iter().map(|b| u32::from(*b)).sum();
    bytes[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
    let checksum = format!("sha256:{:x}", Sha256::digest(&bytes));
    let server = Server::new(bytes, Duration::ZERO);
    let directory = tempfile::tempdir().unwrap();
    let job = serde_json::json!({"target": {"location": server.url, "revision": null, "subdirectory": null, "checksum": checksum},
        "limits": {"source_jobs": 1, "max_download_bytes": 4096, "max_extracted_bytes": 4096, "max_source_files": 100, "source_timeout_seconds": 10}});
    std::fs::write(directory.path().join("job.json"), serde_json::to_vec(&job).unwrap()).unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_paclang"))
        .arg("--acquisition-worker").arg(directory.path()).env_clear()
        .env("HOME", directory.path()).output().unwrap();
    assert!(output.status.success());
    let result: serde_json::Value = serde_json::from_slice(&std::fs::read(directory.path().join("result.json")).unwrap()).unwrap();
    assert_eq!(result["state"], "acquired", "{result}");
    assert!(directory.path().join("tree/日本語:тест.rs").is_file());
}
