use super::{command, fixture_bin, refresh, run_with_inventory};
use rusqlite::Connection;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::time::Duration;

/// Minimal GitHub-compatible Languages API: every GET returns one language
/// breakdown as JSON, like `/repos/{owner}/{repo}/languages` does.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ServerMode {
    Ok,
    /// Every language endpoint answers 403, like an exhausted rate limit.
    RateLimit,
    /// One renamed repository answers 301 to its new path on the same host.
    RedirectSameHost,
    /// The redirect points at another host and must never be followed.
    RedirectCrossHost,
}

struct LanguagesServer {
    url: String,
    hits: Arc<AtomicUsize>,
    auth_hits: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl LanguagesServer {
    fn new() -> Self {
        Self::with_mode(ServerMode::Ok)
    }

    fn with_mode(mode: ServerMode) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let stop = Arc::new(AtomicBool::new(false));
        let hits = Arc::new(AtomicUsize::new(0));
        let auth_hits = Arc::new(AtomicUsize::new(0));
        let (done, count, authed) = (stop.clone(), hits.clone(), auth_hits.clone());
        let own_url = url.clone();
        let thread = std::thread::spawn(move || {
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
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut request = Vec::new();
                while !request.ends_with(b"\r\n\r\n") {
                    let mut byte = [0];
                    if stream.read_exact(&mut byte).is_err() {
                        break;
                    }
                    request.push(byte[0]);
                }
                // Only language endpoints answer; anything else is a 404.
                let text = String::from_utf8_lossy(&request).into_owned();
                if text.contains("Authorization: Bearer secret-token") {
                    authed.fetch_add(1, Ordering::SeqCst);
                }
                let path = text.lines().next().unwrap_or_default().to_owned();
                let (status, extra, body) = match mode {
                    ServerMode::Ok => {
                        if path.contains("/repos/") && path.contains("/languages") {
                            count.fetch_add(1, Ordering::SeqCst);
                            (
                                "200 OK",
                                String::new(),
                                r#"{"Rust": 100, "C": 25}"#.to_owned(),
                            )
                        } else {
                            ("404", String::new(), String::new())
                        }
                    }
                    ServerMode::RateLimit => {
                        if path.contains("/repos/") {
                            count.fetch_add(1, Ordering::SeqCst);
                        }
                        ("403", String::new(), String::new())
                    }
                    ServerMode::RedirectSameHost => {
                        if path.contains("/repos/example/old/languages") {
                            count.fetch_add(1, Ordering::SeqCst);
                            (
                                "301",
                                format!("Location: {own_url}/repos/example/new/languages\r\n"),
                                String::new(),
                            )
                        } else if path.contains("/repos/") {
                            count.fetch_add(1, Ordering::SeqCst);
                            ("200 OK", String::new(), r#"{"Rust": 100}"#.to_owned())
                        } else {
                            ("404", String::new(), String::new())
                        }
                    }
                    ServerMode::RedirectCrossHost => {
                        if path.contains("/repos/") {
                            count.fetch_add(1, Ordering::SeqCst);
                            (
                                "301",
                                "Location: https://example.invalid/repos/evil/repo/languages\r\n"
                                    .to_owned(),
                                String::new(),
                            )
                        } else {
                            ("404", String::new(), String::new())
                        }
                    }
                };
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status}\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        Self {
            url,
            hits,
            auth_hits,
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for LanguagesServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.thread.take().unwrap().join().unwrap();
    }
}

fn seed_resolved(data: &Path, name: &str, version: &str, location: &str, byte: char) {
    seed_resolved_with_provider(data, name, version, "arch", location, byte);
}

fn seed_resolved_with_provider(
    data: &Path,
    name: &str,
    version: &str,
    provider: &str,
    location: &str,
    byte: char,
) {
    let db = Connection::open(data.join("paclang/paclang.db")).unwrap();
    let checksum = format!("sha256:{}", byte.to_string().repeat(64));
    db.execute(
        "INSERT OR IGNORE INTO analysis_targets (location, revision, subdirectory) VALUES (?1, 'tag:v1', '')",
        [location],
    )
    .unwrap();
    db.execute(
        "INSERT INTO provider_attempts (package_name, package_version, provider, outcome, target_id, evidence_grade, recorded_at, source_checksum)
         SELECT ?1, ?2, ?5, 'resolved', id, 'reconstructed', 'old', ?4 FROM analysis_targets WHERE location = ?3",
        rusqlite::params![name, version, location, checksum, provider],
    )
    .unwrap();
}

fn seed_exact_profile(data: &Path, location: &str, byte: char) {
    let db = Connection::open(data.join("paclang/paclang.db")).unwrap();
    let checksum = format!("sha256:{}", byte.to_string().repeat(64));
    db.execute(
        "INSERT INTO language_profiles (target_id, checksum, state, reason, total_bytes, primary_language, languages_json, adapter, source, fidelity, recorded_at)
         SELECT id, ?2, 'analyzed', NULL, 100, 'Rust', '[]', 'github-linguist', 'linguist', 'exact-packaged-source', 'old' FROM analysis_targets WHERE location = ?1",
        rusqlite::params![location, checksum],
    )
    .unwrap();
}

fn profile_source(data: &Path, location: &str) -> (String, String, String) {
    let db = Connection::open(data.join("paclang/paclang.db")).unwrap();
    db.query_row(
        "SELECT state, source, fidelity FROM language_profiles
         JOIN analysis_targets ON analysis_targets.id = language_profiles.target_id
         WHERE location = ?1",
        [location],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )
    .unwrap()
}

#[test]
fn fast_mode_approximates_without_downloading() {
    let data = tempfile::tempdir().unwrap();
    let bin = fixture_bin();
    assert!(
        refresh(&["--format", "json"], data.path(), bin.path())
            .status
            .success()
    );
    seed_resolved(
        data.path(),
        "alpha",
        "1.0-1",
        "https://github.com/example/alpha.git",
        'a',
    );
    seed_resolved(
        data.path(),
        "foreign-tool",
        "3.0-1",
        "https://github.com/example/foreign-tool",
        'b',
    );
    seed_resolved(
        data.path(),
        "suite-cli",
        "2.0-1",
        "https://example.invalid/suite-cli.tar",
        'c',
    );
    // suite-docs stays unresolved.
    let server = LanguagesServer::new();
    let output = command(data.path())
        .args([
            "refresh",
            "--sources-only",
            "--analysis-mode",
            "fast",
            "--github-api-url",
            &server.url,
            "--format",
            "json",
        ])
        .env("PATH", bin.path())
        .env("XDG_CACHE_HOME", data.path().join("cache"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    // Two approximations, never mixed into exact analysis; the tarball and
    // the unresolved package stay missing without any download attempt.
    assert_eq!(report["coverage"]["approximate"], 2);
    assert_eq!(report["coverage"]["analyzed"], 0);
    assert_eq!(report["coverage"]["missing_source_language_analysis"], 2);
    assert_eq!(report["source_acquisition"]["acquired"], 0);
    assert_eq!(report["source_acquisition"]["failed"], 0);
    assert_eq!(server.hits.load(Ordering::SeqCst), 2);
    assert_eq!(
        report["language_approximation"],
        serde_json::json!({
            "attempted": 2,
            "approximated": 2,
            "failed": 0,
            "skipped_exact": 0,
            "skipped_rate_limited": 0,
        })
    );
    assert_eq!(
        profile_source(data.path(), "https://github.com/example/alpha.git"),
        (
            "analyzed".to_owned(),
            "github-api".to_owned(),
            "current-upstream".to_owned()
        )
    );

    let output = run_with_inventory(&["providers", "--format", "json"], data.path(), bin.path());
    let providers: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let alpha = providers["packages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|package| package["name"] == "alpha")
        .unwrap();
    assert_eq!(alpha["attempts"][0]["analysis"]["source"], "github-api");
    assert_eq!(
        alpha["attempts"][0]["analysis"]["fidelity"],
        "current-upstream"
    );
    assert_eq!(alpha["attempts"][0]["analysis"]["primary_language"], "Rust");
}

#[test]
fn circuit_breaker_skips_unfired_requests_without_records() {
    let data = tempfile::tempdir().unwrap();
    let bin = fixture_bin();
    assert!(
        refresh(&["--format", "json"], data.path(), bin.path())
            .status
            .success()
    );
    // Nine distinct targets across the four fixture packages exceed one fetch
    // chunk of eight: the first chunk trips the breaker and the last target
    // never fires.
    let hex: Vec<char> = "abcdef012".chars().collect();
    let mut index = 0;
    for (name, version) in [
        ("alpha", "1.0-1"),
        ("foreign-tool", "3.0-1"),
        ("suite-cli", "2.0-1"),
        ("suite-docs", "2.0-1"),
    ] {
        for provider in ["arch", "cachyos"] {
            seed_resolved_with_provider(
                data.path(),
                name,
                version,
                provider,
                &format!("https://github.com/example/{name}-{provider}.git"),
                hex[index],
            );
            index += 1;
        }
    }
    seed_resolved_with_provider(
        data.path(),
        "alpha",
        "1.0-1",
        "custom",
        "https://github.com/example/alpha-custom.git",
        hex[index],
    );
    let server = LanguagesServer::with_mode(ServerMode::RateLimit);
    let output = command(data.path())
        .args([
            "refresh",
            "--sources-only",
            "--analysis-mode",
            "fast",
            "--github-api-url",
            &server.url,
            "--format",
            "json",
        ])
        .env("PATH", bin.path())
        .env("XDG_CACHE_HOME", data.path().join("cache"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    // Eight requests fired and failed; the ninth never fired and carries no
    // record, so the next refresh retries it instead of fossilizing a failure.
    assert_eq!(server.hits.load(Ordering::SeqCst), 8);
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        report["language_approximation"],
        serde_json::json!({
            "attempted": 8,
            "approximated": 0,
            "failed": 8,
            "skipped_exact": 0,
            "skipped_rate_limited": 1,
        })
    );
    let db = Connection::open(data.path().join("paclang/paclang.db")).unwrap();
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM language_profiles WHERE state = 'failed'",
            [],
            |row| row.get::<_, i64>(0)
        )
        .unwrap(),
        8
    );
}

#[test]
fn token_travels_as_bearer_auth_and_never_otherwise() {
    let data = tempfile::tempdir().unwrap();
    let bin = fixture_bin();
    assert!(
        refresh(&["--format", "json"], data.path(), bin.path())
            .status
            .success()
    );
    seed_resolved(
        data.path(),
        "alpha",
        "1.0-1",
        "https://github.com/example/alpha.git",
        'a',
    );
    let server = LanguagesServer::new();
    let output = command(data.path())
        .args([
            "refresh",
            "--sources-only",
            "--analysis-mode",
            "fast",
            "--github-api-url",
            &server.url,
            "--format",
            "json",
        ])
        .env("PATH", bin.path())
        .env("PACLANG_GITHUB_TOKEN", "secret-token")
        .env("XDG_CACHE_HOME", data.path().join("cache"))
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(server.hits.load(Ordering::SeqCst), 1);
    assert_eq!(server.auth_hits.load(Ordering::SeqCst), 1);

    // Without the variable, no Authorization header is sent at all.
    let data = tempfile::tempdir().unwrap();
    assert!(
        refresh(&["--format", "json"], data.path(), bin.path())
            .status
            .success()
    );
    seed_resolved(
        data.path(),
        "alpha",
        "1.0-1",
        "https://github.com/example/alpha.git",
        'a',
    );
    let server = LanguagesServer::new();
    let output = command(data.path())
        .args([
            "refresh",
            "--sources-only",
            "--analysis-mode",
            "fast",
            "--github-api-url",
            &server.url,
            "--format",
            "json",
        ])
        .env("PATH", bin.path())
        .env_remove("PACLANG_GITHUB_TOKEN")
        .env("XDG_CACHE_HOME", data.path().join("cache"))
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(server.hits.load(Ordering::SeqCst), 1);
    assert_eq!(server.auth_hits.load(Ordering::SeqCst), 0);
}

#[test]
fn same_host_redirect_is_followed_once() {
    let data = tempfile::tempdir().unwrap();
    let bin = fixture_bin();
    assert!(
        refresh(&["--format", "json"], data.path(), bin.path())
            .status
            .success()
    );
    seed_resolved(
        data.path(),
        "alpha",
        "1.0-1",
        "https://github.com/example/old.git",
        'a',
    );
    let server = LanguagesServer::with_mode(ServerMode::RedirectSameHost);
    let output = command(data.path())
        .args([
            "refresh",
            "--sources-only",
            "--analysis-mode",
            "fast",
            "--github-api-url",
            &server.url,
            "--format",
            "json",
        ])
        .env("PATH", bin.path())
        .env("XDG_CACHE_HOME", data.path().join("cache"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    // Initial request plus one same-host hop.
    assert_eq!(server.hits.load(Ordering::SeqCst), 2);
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["coverage"]["approximate"], 1);
    assert_eq!(
        report["language_approximation"],
        serde_json::json!({
            "attempted": 2,
            "approximated": 1,
            "failed": 0,
            "skipped_exact": 0,
            "skipped_rate_limited": 0,
        })
    );
    assert_eq!(
        profile_source(data.path(), "https://github.com/example/old.git"),
        (
            "analyzed".to_owned(),
            "github-api".to_owned(),
            "current-upstream".to_owned()
        )
    );
}

#[test]
fn cross_host_redirect_never_leaves_the_api() {
    let data = tempfile::tempdir().unwrap();
    let bin = fixture_bin();
    assert!(
        refresh(&["--format", "json"], data.path(), bin.path())
            .status
            .success()
    );
    seed_resolved(
        data.path(),
        "alpha",
        "1.0-1",
        "https://github.com/example/evil.git",
        'a',
    );
    let server = LanguagesServer::with_mode(ServerMode::RedirectCrossHost);
    let output = command(data.path())
        .args([
            "refresh",
            "--sources-only",
            "--analysis-mode",
            "fast",
            "--github-api-url",
            &server.url,
            "--format",
            "json",
        ])
        .env("PATH", bin.path())
        .env("XDG_CACHE_HOME", data.path().join("cache"))
        .output()
        .unwrap();
    assert!(output.status.success());
    // Exactly one request fired; the off-host target was never fetched.
    assert_eq!(server.hits.load(Ordering::SeqCst), 1);
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["coverage"]["approximate"], 0);
    let db = Connection::open(data.path().join("paclang/paclang.db")).unwrap();
    assert_eq!(
        db.query_row("SELECT reason FROM language_profiles", [], |row| row
            .get::<_, Option<String>>(0))
            .unwrap()
            .as_deref(),
        Some("GitHub language redirect leaves the API host")
    );
}

#[test]
fn fast_mode_never_overwrites_exact_profiles() {
    let data = tempfile::tempdir().unwrap();
    let bin = fixture_bin();
    assert!(
        refresh(&["--format", "json"], data.path(), bin.path())
            .status
            .success()
    );
    seed_resolved(
        data.path(),
        "alpha",
        "1.0-1",
        "https://github.com/example/alpha.git",
        'a',
    );
    seed_exact_profile(data.path(), "https://github.com/example/alpha.git", 'a');
    let server = LanguagesServer::new();
    let output = command(data.path())
        .args([
            "refresh",
            "--sources-only",
            "--analysis-mode",
            "fast",
            "--github-api-url",
            &server.url,
            "--format",
            "json",
        ])
        .env("PATH", bin.path())
        .env("XDG_CACHE_HOME", data.path().join("cache"))
        .output()
        .unwrap();
    assert!(output.status.success());
    // The exact profile survives; no API call was even needed.
    assert_eq!(server.hits.load(Ordering::SeqCst), 0);
    assert_eq!(
        profile_source(data.path(), "https://github.com/example/alpha.git"),
        (
            "analyzed".to_owned(),
            "linguist".to_owned(),
            "exact-packaged-source".to_owned()
        )
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["coverage"]["analyzed"], 1);
    assert_eq!(report["coverage"]["approximate"], 0);
}

#[test]
fn auto_mode_reuses_exact_approximates_gaps_and_skips_nothing_else() {
    let data = tempfile::tempdir().unwrap();
    let bin = fixture_bin();
    assert!(
        refresh(&["--format", "json"], data.path(), bin.path())
            .status
            .success()
    );
    seed_resolved(
        data.path(),
        "alpha",
        "1.0-1",
        "https://github.com/example/alpha.git",
        'a',
    );
    seed_exact_profile(data.path(), "https://github.com/example/alpha.git", 'a');
    seed_resolved(
        data.path(),
        "foreign-tool",
        "3.0-1",
        "https://github.com/example/foreign-tool",
        'b',
    );
    let server = LanguagesServer::new();
    let output = command(data.path())
        .args([
            "refresh",
            "--sources-only",
            "--analysis-mode",
            "auto",
            "--github-api-url",
            &server.url,
            "--format",
            "json",
        ])
        .env("PATH", bin.path())
        .env("XDG_CACHE_HOME", data.path().join("cache"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["coverage"]["analyzed"], 1);
    assert_eq!(report["coverage"]["approximate"], 1);
    // Both the reused exact target and the freshly approximated one skip
    // acquisition; nothing else was downloadable offline.
    assert_eq!(report["source_acquisition"]["reused"], 2);
    assert_eq!(report["source_acquisition"]["acquired"], 0);
    // One API call for the gap; the exact target was never refetched.
    assert_eq!(server.hits.load(Ordering::SeqCst), 1);
}

#[test]
fn fast_mode_with_unresolved_packages_fails_require_complete() {
    let data = tempfile::tempdir().unwrap();
    let bin = fixture_bin();
    assert!(
        refresh(&["--format", "json"], data.path(), bin.path())
            .status
            .success()
    );
    seed_resolved(
        data.path(),
        "alpha",
        "1.0-1",
        "https://github.com/example/alpha.git",
        'a',
    );
    // Three packages have no resolved source at all.
    let server = LanguagesServer::new();
    let output = command(data.path())
        .args([
            "refresh",
            "--sources-only",
            "--analysis-mode",
            "fast",
            "--github-api-url",
            &server.url,
            "--require-complete",
            "--format",
            "json",
        ])
        .env("PATH", bin.path())
        .env("XDG_CACHE_HOME", data.path().join("cache"))
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["source_acquisition"]["unresolved_packages"], 3);
    assert_eq!(report["coverage"]["approximate"], 1);
}
