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

#[test]
fn riir_uses_source_bytes_and_keeps_memes_out_of_json() {
    let data = tempfile::tempdir().unwrap();
    let bin = fixture_bin();
    assert!(
        refresh(&["--format", "json"], data.path(), bin.path())
            .status
            .success()
    );
    let query = |args: &[&str]| run_with_inventory(args, data.path(), bin.path());
    let empty = query(&["riir", "--format", "json"]);
    let empty: serde_json::Value = serde_json::from_slice(&empty.stdout).unwrap();
    assert!(empty["oxidation_ratio"].is_null());
    assert!(empty["riir_remaining_ratio"].is_null());
    for (name, version) in [("alpha", "1.0-1"), ("suite-cli", "2.0-1")] {
        // Two packages sharing one target must not double global source bytes.
        let db = Connection::open(data.path().join("paclang/paclang.db")).unwrap();
        if name == "alpha" {
            seed_profile(
                data.path(),
                name,
                version,
                "https://example.invalid/shared.git",
                "analyzed",
            );
            db.execute("UPDATE language_profiles SET languages_json = ?1, total_bytes = 120, primary_language = 'Rust'",
                [r#"[{"language":"Rust","bytes":60},{"language":"C","bytes":30},{"language":"C++","bytes":10},{"language":"Python","bytes":20}]"#]).unwrap();
        } else {
            db.execute("INSERT INTO provider_attempts (package_name, package_version, provider, outcome, target_id, evidence_grade, recorded_at, source_checksum)
                SELECT ?1, ?2, provider, outcome, target_id, evidence_grade, recorded_at, source_checksum FROM provider_attempts WHERE package_name = 'alpha'",
                rusqlite::params![name, version]).unwrap();
        }
    }
    let output = query(&["riir", "--format", "json"]);
    assert!(output.status.success());
    assert!(!String::from_utf8_lossy(&output.stdout).contains("🦀"));
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["oxidation_ratio"], 0.6);
    assert_eq!(report["riir_remaining_ratio"], 0.4);
    assert_eq!(report["riir_remaining_bytes"], 40);
    assert_eq!(report["rust_bytes"], 60);
    assert_eq!(report["other_bytes"], 20);
    assert!(report["unknown_source_bytes"].is_null());
    assert_eq!(report["top_rewrite_candidates"][0]["name"], "alpha");
    assert_eq!(report["top_rewrite_candidates"][0]["c_cpp_bytes"], 40);
    assert_eq!(report["coverage_ratio"], 0.5);
    let human = query(&["riir", "--bar", "crabs"]);
    assert!(human.status.success());
    let human = String::from_utf8(human.stdout).unwrap();
    assert!(human.contains("🦀 Rewrite It In Rust 🦀"));
    assert!(human.contains("Ferris grows stronger."));
    assert!(human.trim_end().ends_with("The crabs demand 40 B more."));
    assert_eq!(
        query(&["riir", "--min-coverage", "75"]).status.code(),
        Some(1)
    );
    let filtered = query(&["riir", "--package", "foreign-tool", "--format", "json"]);
    let filtered: serde_json::Value = serde_json::from_slice(&filtered.stdout).unwrap();
    assert!(filtered["oxidation_ratio"].is_null());
    let db = Connection::open(data.path().join("paclang/paclang.db")).unwrap();
    db.execute(
        "UPDATE language_profiles SET fidelity = 'current-upstream', source = 'github-api'",
        [],
    )
    .unwrap();
    let exact: serde_json::Value =
        serde_json::from_slice(&query(&["riir", "--format", "json"]).stdout).unwrap();
    assert!(exact["oxidation_ratio"].is_null());
    let best: serde_json::Value = serde_json::from_slice(
        &query(&["riir", "--fidelity", "best-available", "--format", "json"]).stdout,
    )
    .unwrap();
    assert_eq!(best["oxidation_ratio"], 0.6);
    assert_eq!(best["coverage_ratio"], 0.5);
    assert_eq!(best["exact_coverage_ratio"], 0.0);
    assert_eq!(best["approximate_fallback_coverage_ratio"], 0.5);
    for format in ["toon", "markdown"] {
        assert!(query(&["riir", "--format", format]).status.success());
    }
}

#[test]
fn riir_discloses_mixed_coverage_and_prioritizes_rewrite_volume() {
    let data = tempfile::tempdir().unwrap();
    let bin = fixture_bin();
    assert!(
        refresh(&["--format", "json"], data.path(), bin.path())
            .status
            .success()
    );
    for (name, version, c, rust, approximate) in [
        ("alpha", "1.0-1", 999, 1, false),
        ("foreign-tool", "3.0-1", 80_000, 20_000, true),
        ("suite-cli", "2.0-1", 80_000, 1, false),
    ] {
        let location = format!("https://example.invalid/{name}.git");
        seed_profile(data.path(), name, version, &location, "analyzed");
        let db = Connection::open(data.path().join("paclang/paclang.db")).unwrap();
        db.execute(
            "UPDATE language_profiles SET languages_json = ?1, total_bytes = ?2, source = ?3, fidelity = ?4
             WHERE target_id IN (SELECT id FROM analysis_targets WHERE location = ?5)",
            rusqlite::params![
                format!(r#"[{{"language":"C++","bytes":{c}}},{{"language":"Rust","bytes":{rust}}}]"#),
                c + rust,
                if approximate { "github-api" } else { "linguist" },
                if approximate { "current-upstream" } else { "exact-packaged-source" },
                location,
            ],
        ).unwrap();
    }
    let query = |args: &[&str]| run_with_inventory(args, data.path(), bin.path());
    let output = query(&["riir", "--fidelity", "best-available", "--format", "json"]);
    assert!(output.status.success());
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["coverage_ratio"], 0.75);
    assert_eq!(report["exact_coverage_ratio"], 0.5);
    assert_eq!(report["approximate_fallback_coverage_ratio"], 0.25);
    let candidates = report["top_rewrite_candidates"].as_array().unwrap();
    let names: Vec<_> = candidates
        .iter()
        .map(|row| row["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["foreign-tool", "suite-cli", "alpha"]);
    assert_eq!(candidates[0]["c_cpp_bytes"], 80_000);
    assert_eq!(candidates[0]["c_cpp_ratio"], 0.8);
    for format in ["terminal", "markdown"] {
        let output = query(&["riir", "--fidelity", "best-available", "--format", format]);
        let human = String::from_utf8(output.stdout).unwrap();
        assert!(human.contains(
            "Coverage: 75.0% best-available\n  exact: 50.0%\n  approximate fallback: 25.0%"
        ));
        assert!(human.contains("Other (excluded)"));
        assert!(human.contains("Top rewrite candidates (C/C++ bytes):"));
        assert!(human.contains("78.1 KiB C/C++ (80.0%,"));
        assert!(human.contains("The crabs demand "));
    }
    let exact = query(&["riir", "--format", "json"]);
    let exact: serde_json::Value = serde_json::from_slice(&exact.stdout).unwrap();
    assert_eq!(exact["coverage_ratio"], 0.5);
    assert_eq!(exact["approximate_fallback_coverage_ratio"], 0.0);
    assert_eq!(
        query(&[
            "riir",
            "--fidelity",
            "best-available",
            "--min-coverage",
            "60"
        ])
        .status
        .code(),
        Some(1)
    );
}

#[test]
fn clanker_reuses_deduplication_and_fidelity_and_preserves_full_rows() {
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
        "https://example.invalid/exact.git",
        "analyzed",
    );
    seed_profile(
        data.path(),
        "foreign-tool",
        "3.0-1",
        "https://example.invalid/approx.git",
        "analyzed",
    );
    let db = Connection::open(data.path().join("paclang/paclang.db")).unwrap();
    db.execute("UPDATE language_profiles SET languages_json = ?1, total_bytes = 6000 WHERE target_id IN (SELECT id FROM analysis_targets WHERE location = 'https://example.invalid/exact.git')",
        [r#"[{"language":"Rust","bytes":10},{"language":"C","bytes":90},{"language":"Perl","bytes":900},{"language":"HTML","bytes":5000}]"#]).unwrap();
    db.execute("UPDATE language_profiles SET languages_json = ?1, total_bytes = 100, source = 'github-api', fidelity = 'current-upstream' WHERE target_id IN (SELECT id FROM analysis_targets WHERE location = 'https://example.invalid/approx.git')",
        [r#"[{"language":"Ruby","bytes":100}]"#]).unwrap();
    // A split package shares the exact target. Alpha also has an approximate
    // alternative; the existing source-byte policy must prefer its exact target.
    db.execute("INSERT INTO provider_attempts (package_name, package_version, provider, outcome, target_id, evidence_grade, recorded_at, source_checksum)
        SELECT 'suite-cli', '2.0-1', provider, outcome, target_id, evidence_grade, recorded_at, source_checksum FROM provider_attempts WHERE package_name = 'alpha'", []).unwrap();
    db.execute("INSERT INTO provider_attempts (package_name, package_version, provider, outcome, target_id, evidence_grade, recorded_at, source_checksum)
        SELECT 'alpha', '1.0-1', 'cachyos', outcome, target_id, evidence_grade, recorded_at, source_checksum FROM provider_attempts WHERE package_name = 'foreign-tool'", []).unwrap();
    let query = |args: &[&str]| run_with_inventory(args, data.path(), bin.path());
    for fidelity in ["exact", "best-available"] {
        let output = query(&[
            "clanker-index",
            "--fidelity",
            fidelity,
            "--top",
            "1",
            "--format",
            "json",
        ]);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        let source: serde_json::Value = serde_json::from_slice(
            &query(&["source-bytes", "--fidelity", fidelity, "--format", "json"]).stdout,
        )
        .unwrap();
        assert_eq!(report["source_analysis"], source);
        assert_eq!(report["ungraded_programming_bytes"], 900);
        assert_eq!(report["ancillary_bytes"], 5000);
        assert_eq!(report["ancillary_languages"][0]["language"], "HTML");
        assert_eq!(report["unknown_type_bytes"], 0);
        assert_eq!(report["policy"], "clanker-language-tierlist-v1");
        assert_eq!(
            report["languages"].as_array().unwrap().len(),
            if fidelity == "exact" { 3 } else { 4 }
        );
        assert_eq!(report["grade"], if fidelity == "exact" { "C" } else { "D" });
        assert_eq!(
            report["total_source_bytes"],
            if fidelity == "exact" { 6000 } else { 6100 }
        );
        let human = String::from_utf8(
            query(&["clanker-index", "--fidelity", fidelity, "--top", "1"]).stdout,
        )
        .unwrap();
        assert!(human.contains("Other programming languages (display-only, mixed tiers;"));
        assert!(human.contains("🐪 Perl"));
        assert!(!human.contains("HTML"));
        assert!(human.contains("Ancillary source bytes:       5.0 KB"));
        assert!(human.contains("immediate investigation required"));
        assert!(human.contains("Ungraded programming bytes:   900 B"));
    }
    let selected: serde_json::Value = serde_json::from_slice(
        &query(&[
            "clanker-index",
            "--fidelity",
            "best-available",
            "--package",
            "alpha",
            "--format",
            "json",
        ])
        .stdout,
    )
    .unwrap();
    assert_eq!(selected["total_source_bytes"], 6000);
    assert_eq!(selected["grade"], "C");
    for args in [
        vec!["clanker-index", "--all", "--top", "1"],
        vec!["clanker-index", "--top", "0"],
    ] {
        assert_eq!(query(&args).status.code(), Some(2));
    }
    assert_eq!(
        query(&["clanker-index", "--min-coverage", "100"])
            .status
            .code(),
        Some(1)
    );
    for format in ["toon", "markdown"] {
        assert!(
            query(&["clanker-index", "--all", "--format", format])
                .status
                .success()
        );
    }
}

#[test]
fn clanker_discloses_stale_analysis_and_handles_empty_population() {
    let data = tempfile::tempdir().unwrap();
    let bin = fixture_bin();
    assert!(
        refresh(&["--format", "json"], data.path(), bin.path())
            .status
            .success()
    );
    let query = |args: &[&str]| run_with_inventory(args, data.path(), bin.path());
    let empty: serde_json::Value =
        serde_json::from_slice(&query(&["clanker-index", "--format", "json"]).stdout).unwrap();
    assert!(empty["grade"].is_null());
    assert_eq!(empty["gradeable_bytes"], 0);
    seed_profile(
        data.path(),
        "alpha",
        "1.0-1",
        "https://example.invalid/stale.git",
        "analyzed",
    );
    let db = Connection::open(data.path().join("paclang/paclang.db")).unwrap();
    db.execute(
        "UPDATE language_profiles SET languages_json = ?1",
        [r#"[{"language":"Rust","bytes":100}]"#],
    )
    .unwrap();
    upgrade_alpha_to_1_1(bin.path());
    assert!(
        refresh(&["--format", "json"], data.path(), bin.path())
            .status
            .success()
    );
    let report: serde_json::Value =
        serde_json::from_slice(&query(&["clanker-index", "--format", "json"]).stdout).unwrap();
    assert_eq!(report["grade"], "S+");
    assert_eq!(report["source_analysis"]["stale_source_bytes"], 100);
    assert_eq!(report["source_analysis"]["coverage"]["stale"], 1);
    let human = String::from_utf8(query(&["clanker-index"]).stdout).unwrap();
    assert!(human.contains("stale source bytes (included): 100 B"));
}
