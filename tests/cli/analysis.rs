use super::{command, fixture_bin, refresh, run_with_inventory, write_executable};
use rusqlite::Connection;
use std::path::Path;

fn checksum(byte: char) -> String {
    format!("sha256:{}", byte.to_string().repeat(64))
}

/// Record a resolved attempt with a stored language profile for one package
/// version, bypassing network fetchers. The profile state drives the
/// package-level analysis state under test.
fn seed_profile(data: &Path, name: &str, version: &str, location: &str, state: &str) {
    let db = Connection::open(data.join("paclang/paclang.db")).unwrap();
    let sum = checksum(if version == "1.0-1" { 'a' } else { 'b' });
    db.execute(
        "INSERT OR IGNORE INTO analysis_targets (location, revision, subdirectory) VALUES (?1, 'tag:v1', '')",
        [location],
    )
    .unwrap();
    db.execute(
        "INSERT INTO provider_attempts (package_name, package_version, provider, outcome, target_id, evidence_grade, recorded_at, source_checksum)
         SELECT ?1, ?2, 'arch', 'resolved', id, 'reconstructed', 'old', ?4 FROM analysis_targets WHERE location = ?3",
        rusqlite::params![name, version, location, sum],
    )
    .unwrap();
    let primary: Option<&str> = (state == "analyzed").then_some("Rust");
    db.execute(
        "INSERT INTO language_profiles (target_id, checksum, state, reason, total_bytes, primary_language, languages_json, adapter, recorded_at)
         SELECT id, ?4, ?5, NULL, 100, ?6, '[]', 'github-linguist', 'old' FROM analysis_targets WHERE location = ?3",
        rusqlite::params![location, location, location, sum, state, primary],
    )
    .unwrap();
}

fn upgrade_alpha_to_1_1(bin: &Path) {
    write_executable(
        &bin.join("pacman"),
        "#!/bin/sh\n\
         [ \"$#\" -eq 1 ] || exit 64\n\
         if [ \"$1\" = '-Qq' ]; then\n\
           printf 'alpha\\nforeign-tool\\nsuite-cli\\nsuite-docs\\n'\n\
         elif [ \"$1\" = '-Q' ]; then\n\
           printf 'alpha 1.1-1\\nforeign-tool 3.0-1\\nsuite-cli 2.0-1\\nsuite-docs 2.0-1\\n'\n\
         else\n\
           exit 64\n\
         fi\n",
    );
    write_executable(
        &bin.join("expac"),
        "#!/bin/sh\n\
         [ \"$#\" -eq 2 ] && [ \"$1\" = '-Q' ] || exit 64\n\
         printf 'alpha\\t1.1-1\\talpha\\tlocal\\texplicit\\t120\\n\
         foreign-tool\\t3.0-1\\tforeign-tool\\tlocal\\texplicit\\t40\\n\
         suite-cli\\t2.0-1\\tsuite\\tlocal\\tdependency\\t300\\n\
         suite-docs\\t2.0-1\\tsuite\\tlocal\\tdependency\\t80\\n'\n",
    );
}

fn count(data: &Path, bin: &Path) -> serde_json::Value {
    let output = run_with_inventory(&["count", "--format", "json"], data, bin);
    assert!(output.status.success());
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn upgrade_without_new_analysis_reports_stale_but_includes_it() {
    let data = tempfile::tempdir().unwrap();
    let bin = fixture_bin();
    assert!(
        refresh(&["--format", "json"], data.path(), bin.path())
            .status
            .success()
    );
    seed_profile(
        data.path(),
        "alpha",
        "1.0-1",
        "https://example.invalid/alpha.git",
        "analyzed",
    );
    upgrade_alpha_to_1_1(bin.path());
    assert!(
        command(data.path())
            .args(["refresh", "--metadata-only", "--format", "json"])
            .env("PATH", bin.path())
            .output()
            .unwrap()
            .status
            .success()
    );

    let report = count(data.path(), bin.path());
    assert_eq!(report["selected_population"], 4);
    assert_eq!(report["coverage"]["denominator"], 4);
    // Stale stays included in the analyzed share, disclosed separately.
    assert_eq!(report["coverage"]["analyzed"], 1);
    assert_eq!(report["coverage"]["stale"], 1);
    assert_eq!(report["coverage"]["missing_source_language_analysis"], 3);

    let output = run_with_inventory(&["providers", "--format", "json"], data.path(), bin.path());
    let providers: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let alpha = providers["packages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|package| package["name"] == "alpha")
        .unwrap();
    assert_eq!(alpha["stale"], true);
}

#[test]
fn fresh_analysis_on_the_new_version_is_current_not_stale() {
    let data = tempfile::tempdir().unwrap();
    let bin = fixture_bin();
    assert!(
        refresh(&["--format", "json"], data.path(), bin.path())
            .status
            .success()
    );
    seed_profile(
        data.path(),
        "alpha",
        "1.0-1",
        "https://example.invalid/alpha-v1.git",
        "analyzed",
    );
    upgrade_alpha_to_1_1(bin.path());
    assert!(
        command(data.path())
            .args(["refresh", "--metadata-only", "--format", "json"])
            .env("PATH", bin.path())
            .output()
            .unwrap()
            .status
            .success()
    );
    seed_profile(
        data.path(),
        "alpha",
        "1.1-1",
        "https://example.invalid/alpha-v2.git",
        "analyzed",
    );

    let report = count(data.path(), bin.path());
    assert_eq!(report["coverage"]["analyzed"], 1);
    assert_eq!(report["coverage"]["stale"], 0);
}

#[test]
fn definitive_no_language_on_the_new_version_suppresses_stale() {
    let data = tempfile::tempdir().unwrap();
    let bin = fixture_bin();
    assert!(
        refresh(&["--format", "json"], data.path(), bin.path())
            .status
            .success()
    );
    seed_profile(
        data.path(),
        "alpha",
        "1.0-1",
        "https://example.invalid/alpha-v1.git",
        "analyzed",
    );
    upgrade_alpha_to_1_1(bin.path());
    assert!(
        command(data.path())
            .args(["refresh", "--metadata-only", "--format", "json"])
            .env("PATH", bin.path())
            .output()
            .unwrap()
            .status
            .success()
    );
    // The new source was analyzed and has no detectable language: the old
    // Rust profile must not leak back in as stale.
    seed_profile(
        data.path(),
        "alpha",
        "1.1-1",
        "https://example.invalid/alpha-v2.git",
        "no_language",
    );

    let report = count(data.path(), bin.path());
    assert_eq!(report["coverage"]["analyzed"], 0);
    assert_eq!(report["coverage"]["stale"], 0);
    assert_eq!(report["coverage"]["missing_source_language_analysis"], 4);
}

#[test]
fn failed_current_analysis_falls_back_to_stale() {
    let data = tempfile::tempdir().unwrap();
    let bin = fixture_bin();
    assert!(
        refresh(&["--format", "json"], data.path(), bin.path())
            .status
            .success()
    );
    seed_profile(
        data.path(),
        "alpha",
        "1.0-1",
        "https://example.invalid/alpha-v1.git",
        "analyzed",
    );
    upgrade_alpha_to_1_1(bin.path());
    assert!(
        command(data.path())
            .args(["refresh", "--metadata-only", "--format", "json"])
            .env("PATH", bin.path())
            .output()
            .unwrap()
            .status
            .success()
    );
    seed_profile(
        data.path(),
        "alpha",
        "1.1-1",
        "https://example.invalid/alpha-v2.git",
        "failed",
    );

    let report = count(data.path(), bin.path());
    assert_eq!(report["coverage"]["analyzed"], 1);
    assert_eq!(report["coverage"]["stale"], 1);
}

#[test]
fn packages_language_modes_fidelity_and_filters() {
    let data = tempfile::tempdir().unwrap();
    let bin = fixture_bin();
    assert!(
        refresh(&["--format", "json"], data.path(), bin.path())
            .status
            .success()
    );
    seed_profile(
        data.path(),
        "alpha",
        "1.0-1",
        "https://example.invalid/alpha.git",
        "analyzed",
    );
    let db = Connection::open(data.path().join("paclang/paclang.db")).unwrap();
    db.execute(
        "UPDATE language_profiles SET languages_json = ?1, primary_language = 'C++'",
        [r#"[{"language":"C++","bytes":60},{"language":"Rust","bytes":40}]"#],
    )
    .unwrap();
    let query = |args: &[&str]| {
        let mut command = vec!["packages"];
        command.extend_from_slice(args);
        command.extend_from_slice(&["--format", "json"]);
        let output = run_with_inventory(&command, data.path(), bin.path());
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap()
    };
    assert_eq!(query(&["rust"])["matched_packages"], 0);
    assert_eq!(query(&["c++"])["packages"][0]["name"], "alpha");
    assert_eq!(query(&["RUST", "--contains"])["matched_packages"], 1);
    assert_eq!(
        query(&["rust", "--min-share", "0.4"])["matched_packages"],
        1
    );
    assert_eq!(
        query(&["rust", "--min-share", "0.5"])["matched_packages"],
        0
    );
    assert_eq!(
        query(&["rust", "--contains", "--dependency"])["matched_packages"],
        0
    );
    db.execute(
        "UPDATE language_profiles SET fidelity = 'current-upstream', source = 'github-api'",
        [],
    )
    .unwrap();
    assert_eq!(query(&["rust", "--contains"])["matched_packages"], 0);
    let report = query(&["rust", "--contains", "--fidelity", "best-available"]);
    assert_eq!(report["packages"][0]["fidelity"], "current-upstream");
    for args in [
        vec!["rust", "--min-share", "NaN"],
        vec!["rust", "--min-share", "1.1"],
        vec!["rust", "--contains", "--min-share", "0.5"],
        vec!["rust", "--fidelity", "approximate"],
    ] {
        let mut command = vec!["packages"];
        command.extend(args);
        assert_eq!(
            run_with_inventory(&command, data.path(), bin.path())
                .status
                .code(),
            Some(2)
        );
    }
    for format in ["terminal", "markdown", "toon"] {
        assert!(
            run_with_inventory(
                &[
                    "packages",
                    "c++",
                    "--fidelity",
                    "best-available",
                    "--format",
                    format
                ],
                data.path(),
                bin.path()
            )
            .status
            .success()
        );
    }
}

#[test]
fn packages_sort_by_selected_language_share_and_bytes() {
    let data = tempfile::tempdir().unwrap();
    let bin = fixture_bin();
    assert!(
        refresh(&["--format", "json"], data.path(), bin.path())
            .status
            .success()
    );
    for (name, version, rust, other) in [
        ("alpha", "1.0-1", 600, 400),
        ("foreign-tool", "3.0-1", 90, 10),
        ("suite-cli", "2.0-1", 600, 400),
    ] {
        let location = format!("https://example.invalid/{name}.git");
        seed_profile(data.path(), name, version, &location, "analyzed");
        let db = Connection::open(data.path().join("paclang/paclang.db")).unwrap();
        db.execute(
            "UPDATE language_profiles SET languages_json = ?1, total_bytes = ?2 WHERE target_id IN (SELECT id FROM analysis_targets WHERE location = ?3)",
            rusqlite::params![
                format!(r#"[{{"language":"Rust","bytes":{rust}}},{{"language":"C","bytes":{other}}}]"#),
                rust + other, location
            ],
        ).unwrap();
    }
    for (sort, expected) in [
        ("name", vec!["alpha", "foreign-tool", "suite-cli"]),
        ("share", vec!["foreign-tool", "alpha", "suite-cli"]),
        ("bytes", vec!["alpha", "suite-cli", "foreign-tool"]),
    ] {
        let output = run_with_inventory(
            &["packages", "rust", "--sort", sort, "--format", "json"],
            data.path(),
            bin.path(),
        );
        assert!(output.status.success());
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(report["sort"], sort);
        let names: Vec<_> = report["packages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, expected);
    }
    assert_eq!(
        run_with_inventory(
            &["packages", "rust", "--sort", "invalid"],
            data.path(),
            bin.path()
        )
        .status
        .code(),
        Some(2)
    );
}
