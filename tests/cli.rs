#[path = "cli/analysis.rs"]
mod analysis;
#[path = "cli/fast.rs"]
mod fast;
#[path = "cli/recipes.rs"]
mod recipes;
#[path = "cli/sources.rs"]
mod sources;

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use nix::pty::openpty;
use rusqlite::Connection;

fn command(data_home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_paclang"));
    command
        .env("XDG_DATA_HOME", data_home)
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_CACHE_HOME");
    command
}

fn run(arguments: &[&str], data_home: &Path) -> Output {
    command(data_home).args(arguments).output().unwrap()
}

fn run_with_inventory(arguments: &[&str], data_home: &Path, fixture_bin: &Path) -> Output {
    command(data_home)
        .args(arguments)
        .env("PATH", fixture_bin)
        .output()
        .unwrap()
}

fn write_executable(path: &Path, contents: &str) {
    std::fs::write(path, contents).unwrap();
    let mut permissions = std::fs::metadata(path).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(path, permissions).unwrap();
}

fn fixture_bin() -> tempfile::TempDir {
    let directory = tempfile::tempdir().unwrap();
    write_executable(
        &directory.path().join("pacman"),
        "#!/bin/sh\n\
         [ \"$#\" -eq 1 ] || exit 64\n\
         if [ \"$1\" = '-Qq' ]; then\n\
           printf 'alpha\\nforeign-tool\\nsuite-cli\\nsuite-docs\\n'\n\
         elif [ \"$1\" = '-Q' ]; then\n\
           printf 'alpha 1.0-1\\nforeign-tool 3.0-1\\nsuite-cli 2.0-1\\nsuite-docs 2.0-1\\n'\n\
         else\n\
           exit 64\n\
         fi\n",
    );
    write_executable(
        &directory.path().join("expac"),
        "#!/bin/sh\n\
         [ \"$#\" -eq 2 ] && [ \"$1\" = '-Q' ] || exit 64\n\
         [ \"$2\" = '%n\\t%v\\t%e\\t%r\\t%w\\t%m' ] || exit 65\n\
         printf 'alpha\\t1.0-1\\talpha\\tlocal\\texplicit\\t120\\n\
foreign-tool\\t3.0-1\\tforeign-tool\\tlocal\\texplicit\\t40\\n\
suite-cli\\t2.0-1\\tsuite\\tlocal\\tdependency\\t300\\n\
suite-docs\\t2.0-1\\tsuite\\tlocal\\tdependency\\t80\\n'\n",
    );
    directory
}

fn refresh(arguments: &[&str], data_home: &Path, fixture_bin: &Path) -> Output {
    command(data_home)
        .arg("refresh")
        .arg("--metadata-only")
        .args(arguments)
        .env("PATH", fixture_bin)
        .output()
        .unwrap()
}

fn assign_fixture_origins(data_home: &Path) {
    let connection = Connection::open(data_home.join("paclang/paclang.db")).unwrap();
    for (name, origin) in [
        ("alpha", "core"),
        ("foreign-tool", "foreign"),
        ("suite-cli", "extra"),
        ("suite-docs", "extra"),
    ] {
        connection
            .execute(
                "UPDATE installed_packages SET origin = ?1 WHERE name = ?2",
                [origin, name],
            )
            .unwrap();
    }
}

fn read_pty(reader: &mut File) -> String {
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 1024];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => bytes.extend_from_slice(&buffer[..count]),
            Err(error) if error.raw_os_error() == Some(nix::libc::EIO) => break,
            Err(error) => panic!("cannot read PTY output: {error}"),
        }
    }
    String::from_utf8(bytes).unwrap()
}

fn snapshot_count(data_home: &Path) -> u32 {
    Connection::open(data_home.join("paclang/paclang.db"))
        .unwrap()
        .query_row("SELECT count(*) FROM inventory_snapshots", [], |row| {
            row.get(0)
        })
        .unwrap()
}

fn wait_for_file(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !path.is_file() {
        assert!(Instant::now() < deadline, "timed out waiting for {path:?}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn plain_command_returns_toon_no_snapshot_and_migrates_database() {
    let directory = tempfile::tempdir().unwrap();
    let output = run(&[], directory.path());

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        include_str!("fixtures/no_snapshot.toon")
    );
    assert!(output.stderr.is_empty());

    let connection = Connection::open(directory.path().join("paclang/paclang.db")).unwrap();
    let version: u32 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(version, 11);
}

#[test]
fn plain_command_and_count_share_the_report_path() {
    let directory = tempfile::tempdir().unwrap();
    let plain = run(&["--format", "json"], directory.path());
    let count = run(&["count", "--format", "json"], directory.path());

    assert_eq!(plain.status.code(), Some(1));
    assert_eq!(count.status.code(), Some(1));
    assert_eq!(plain.stdout, count.stdout);
}

#[test]
fn explicit_formats_match_error_goldens() {
    let cases = [
        ("terminal", include_str!("fixtures/no_snapshot.terminal")),
        ("toon", include_str!("fixtures/no_snapshot.toon")),
        ("json", include_str!("fixtures/no_snapshot.json")),
        ("markdown", include_str!("fixtures/no_snapshot.md")),
    ];

    for (format, golden) in cases {
        let directory = tempfile::tempdir().unwrap();
        let output = run(&["--format", format], directory.path());

        assert_eq!(output.status.code(), Some(1), "format: {format}");
        assert_eq!(String::from_utf8(output.stdout).unwrap(), golden);
        assert!(output.stderr.is_empty());
    }
}

#[test]
fn tty_stdout_selects_terminal_format() {
    let directory = tempfile::tempdir().unwrap();
    let terminal = openpty(None, None).unwrap();
    let mut reader = File::from(terminal.master);
    let writer = File::from(terminal.slave);
    let mut child = command(directory.path())
        .stdout(Stdio::from(writer))
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let status = child.wait().unwrap();
    let stdout = read_pty(&mut reader);
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();

    assert_eq!(status.code(), Some(1));
    assert_eq!(
        stdout.replace("\r\n", "\n"),
        include_str!("fixtures/no_snapshot.terminal")
    );
    assert!(stderr.is_empty());
}

#[test]
fn invalid_cli_use_is_structured_and_exits_two() {
    let cases = [
        (&["invented", "--format", "json"][..], "json"),
        (&["--format", "markdown", "--invented"][..], "markdown"),
        (&["--format", "xml"][..], "toon"),
    ];

    for (arguments, expected_format) in cases {
        let directory = tempfile::tempdir().unwrap();
        let output = run(arguments, directory.path());
        let stdout = String::from_utf8(output.stdout).unwrap();

        assert_eq!(output.status.code(), Some(2));
        assert!(output.stderr.is_empty());
        match expected_format {
            "json" => {
                let value: serde_json::Value = serde_json::from_str(&stdout).unwrap();
                assert_eq!(value["schema_version"], "1.5.0");
                assert_eq!(value["error"]["code"], "invalid_cli");
            }
            "markdown" => assert!(stdout.contains("- Code: `invalid_cli`")),
            "toon" => {
                let value: serde_json::Value = toon_format::decode_default(&stdout).unwrap();
                assert_eq!(value["schema_version"], "1.5.0");
                assert_eq!(value["error"]["code"], "invalid_cli");
            }
            _ => unreachable!(),
        }
    }
}

#[test]
fn failed_stdout_write_exits_one_without_panicking() {
    let directory = tempfile::tempdir().unwrap();
    let full = OpenOptions::new().write(true).open("/dev/full").unwrap();
    let output = command(directory.path())
        .args(["--format", "json"])
        .stdout(Stdio::from(full))
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(1));
}

#[test]
fn help_exits_successfully() {
    let directory = tempfile::tempdir().unwrap();
    let output = run(&["--help"], directory.path());

    assert_eq!(output.status.code(), Some(0));
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .contains("Usage: paclang")
    );
}

#[test]
fn home_is_the_fallback_data_directory() {
    let directory = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_paclang"))
        .arg("--format=json")
        .env_remove("XDG_DATA_HOME")
        .env("HOME", directory.path())
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(1));
    assert!(
        directory
            .path()
            .join(".local/share/paclang/paclang.db")
            .is_file()
    );
}

#[test]
fn relative_xdg_data_home_uses_the_home_fallback() {
    let directory = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_paclang"))
        .arg("--format=json")
        .env("XDG_DATA_HOME", "relative/data")
        .env("HOME", directory.path())
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        include_str!("fixtures/no_snapshot.json")
    );
    assert!(output.stderr.is_empty());
    assert!(
        directory
            .path()
            .join(".local/share/paclang/paclang.db")
            .is_file()
    );
}

#[test]
fn metadata_refresh_and_count_work_in_every_format() {
    let cases = [
        ("terminal", include_str!("fixtures/count.terminal")),
        ("toon", include_str!("fixtures/count.toon")),
        ("json", include_str!("fixtures/count.json")),
        ("markdown", include_str!("fixtures/count.md")),
    ];

    for (format, golden) in cases {
        let data_home = tempfile::tempdir().unwrap();
        let fixture_bin = fixture_bin();
        let output = refresh(&["--format", format], data_home.path(), fixture_bin.path());
        let stdout = String::from_utf8(output.stdout).unwrap();

        assert_eq!(output.status.code(), Some(0), "format: {format}");
        assert!(output.stderr.is_empty(), "format: {format}");
        assert_eq!(stdout, golden, "format: {format}");
    }
}

#[test]
fn refresh_does_not_run_a_fallible_live_probe_after_publication() {
    let data_home = tempfile::tempdir().unwrap();
    let fixture_bin = fixture_bin();
    let state = data_home.path().join("probe-count");
    std::fs::write(&state, "0\n").unwrap();
    write_executable(
        &fixture_bin.path().join("pacman"),
        "#!/bin/sh\n\
         [ \"$#\" -eq 1 ] || exit 64\n\
         if [ \"$1\" = '-Qq' ]; then\n\
           printf 'alpha\\nforeign-tool\\nsuite-cli\\nsuite-docs\\n'\n\
         elif [ \"$1\" = '-Q' ]; then\n\
           IFS= read -r count < \"$PACLANG_STATE\"\n\
           count=$((count + 1))\n\
           printf '%s\\n' \"$count\" > \"$PACLANG_STATE\"\n\
           [ \"$count\" -le 2 ] || exit 70\n\
           printf 'alpha 1.0-1\\nforeign-tool 3.0-1\\nsuite-cli 2.0-1\\nsuite-docs 2.0-1\\n'\n\
         else\n\
           exit 64\n\
         fi\n",
    );

    let output = command(data_home.path())
        .args(["refresh", "--metadata-only", "--format", "json"])
        .env("PATH", fixture_bin.path())
        .env("PACLANG_STATE", &state)
        .output()
        .unwrap();

    assert!(output.status.success());
    assert_eq!(std::fs::read_to_string(state).unwrap(), "2\n");
    assert_eq!(snapshot_count(data_home.path()), 1);
}

#[test]
fn count_compares_the_snapshot_with_the_live_inventory() {
    let data_home = tempfile::tempdir().unwrap();
    let fixture_bin = fixture_bin();
    let refreshed = refresh(&["--format", "json"], data_home.path(), fixture_bin.path());
    assert_eq!(refreshed.status.code(), Some(0));

    let output = run_with_inventory(
        &["count", "--format", "json"],
        data_home.path(),
        fixture_bin.path(),
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(value["installed_total"], 4);
    assert_eq!(value["snapshot_staleness"], "current");
}

#[test]
fn package_upgrade_marks_the_published_snapshot_stale() {
    let data_home = tempfile::tempdir().unwrap();
    let fixture_bin = fixture_bin();
    assert!(
        refresh(&["--format", "json"], data_home.path(), fixture_bin.path())
            .status
            .success()
    );
    write_executable(
        &fixture_bin.path().join("pacman"),
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

    let output = run_with_inventory(
        &["count", "--format", "json"],
        data_home.path(),
        fixture_bin.path(),
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();

    assert!(output.status.success());
    assert_eq!(report["snapshot_staleness"], "stale");
    assert_eq!(report["installed_total"], 4);
}

#[test]
fn inventory_change_during_refresh_publishes_one_coherent_stale_snapshot() {
    let data_home = tempfile::tempdir().unwrap();
    let fixture_bin = fixture_bin();
    assert!(
        refresh(&["--format", "json"], data_home.path(), fixture_bin.path())
            .status
            .success()
    );
    let state = data_home.path().join("fingerprint-state");
    std::fs::write(&state, "0\n").unwrap();
    write_executable(
        &fixture_bin.path().join("pacman"),
        "#!/bin/sh\n\
         [ \"$#\" -eq 1 ] || exit 64\n\
         if [ \"$1\" = '-Qq' ]; then\n\
           printf 'alpha\\nforeign-tool\\nsuite-cli\\nsuite-docs\\n'\n\
         elif [ \"$1\" = '-Q' ]; then\n\
           IFS= read -r count < \"$PACLANG_STATE\"\n\
           count=$((count + 1))\n\
           printf '%s\\n' \"$count\" > \"$PACLANG_STATE\"\n\
           version=1.0-1\n\
           [ \"$count\" -eq 1 ] || version=1.1-1\n\
           printf 'alpha %s\\nforeign-tool 3.0-1\\nsuite-cli 2.0-1\\nsuite-docs 2.0-1\\n' \"$version\"\n\
         else\n\
           exit 64\n\
         fi\n",
    );

    let refreshed = command(data_home.path())
        .args(["refresh", "--metadata-only", "--format", "json"])
        .env("PATH", fixture_bin.path())
        .env("PACLANG_STATE", &state)
        .output()
        .unwrap();
    let report: serde_json::Value = serde_json::from_slice(&refreshed.stdout).unwrap();

    assert!(refreshed.status.success());
    assert_eq!(report["snapshot_staleness"], "stale");
    assert_eq!(snapshot_count(data_home.path()), 2);
}

#[test]
fn metadata_matching_neither_boundary_fingerprint_does_not_publish() {
    let data_home = tempfile::tempdir().unwrap();
    let fixture_bin = fixture_bin();
    assert!(
        refresh(&["--format", "json"], data_home.path(), fixture_bin.path())
            .status
            .success()
    );
    let state = data_home.path().join("mixed-fingerprint-state");
    std::fs::write(&state, "0\n").unwrap();
    write_executable(
        &fixture_bin.path().join("pacman"),
        "#!/bin/sh\n\
         [ \"$#\" -eq 1 ] || exit 64\n\
         if [ \"$1\" = '-Qq' ]; then\n\
           printf 'alpha\\nforeign-tool\\nsuite-cli\\nsuite-docs\\n'\n\
         elif [ \"$1\" = '-Q' ]; then\n\
           IFS= read -r count < \"$PACLANG_STATE\"\n\
           count=$((count + 1))\n\
           printf '%s\\n' \"$count\" > \"$PACLANG_STATE\"\n\
           version=0.9-1\n\
           [ \"$count\" -eq 1 ] || version=1.1-1\n\
           printf 'alpha %s\\nforeign-tool 3.0-1\\nsuite-cli 2.0-1\\nsuite-docs 2.0-1\\n' \"$version\"\n\
         else\n\
           exit 64\n\
         fi\n",
    );

    let failed = command(data_home.path())
        .args(["refresh", "--metadata-only", "--format", "json"])
        .env("PATH", fixture_bin.path())
        .env("PACLANG_STATE", &state)
        .output()
        .unwrap();
    let error: serde_json::Value = serde_json::from_slice(&failed.stdout).unwrap();

    assert_eq!(failed.status.code(), Some(1));
    assert_eq!(error["error"]["code"], "inventory");
    assert_eq!(snapshot_count(data_home.path()), 1);
}

#[test]
fn count_filters_individual_install_reasons() {
    let data_home = tempfile::tempdir().unwrap();
    let fixture_bin = fixture_bin();
    assert!(
        refresh(&["--format", "json"], data_home.path(), fixture_bin.path())
            .status
            .success()
    );

    for (filter, expected_reason) in [("--explicit", "explicit"), ("--dependency", "dependency")] {
        let output = run_with_inventory(
            &["count", filter, "--format", "json"],
            data_home.path(),
            fixture_bin.path(),
        );
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();

        assert!(output.status.success(), "filter: {filter}");
        assert_eq!(report["selection"]["install_reason"], expected_reason);
        assert_eq!(report["selected_population"], 2);
        assert_eq!(report["coverage"]["denominator"], 2);
        assert_eq!(report["coverage"]["missing_source_language_analysis"], 2);
    }
}

#[test]
fn repeated_origins_select_their_union() {
    let data_home = tempfile::tempdir().unwrap();
    let fixture_bin = fixture_bin();
    assert!(
        refresh(&["--format", "json"], data_home.path(), fixture_bin.path())
            .status
            .success()
    );
    assign_fixture_origins(data_home.path());

    let output = run_with_inventory(
        &[
            "count", "--origin", "extra", "--origin", "core", "--format", "json",
        ],
        data_home.path(),
        fixture_bin.path(),
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();

    assert!(output.status.success());
    assert_eq!(
        report["selection"]["origins"],
        serde_json::json!(["core", "extra"])
    );
    assert_eq!(report["selected_population"], 3);
}

#[test]
fn repeated_packages_select_their_union() {
    let data_home = tempfile::tempdir().unwrap();
    let fixture_bin = fixture_bin();
    assert!(
        refresh(&["--format", "json"], data_home.path(), fixture_bin.path())
            .status
            .success()
    );

    let output = run_with_inventory(
        &[
            "count",
            "--package",
            "suite-docs",
            "--package",
            "alpha",
            "--format",
            "json",
        ],
        data_home.path(),
        fixture_bin.path(),
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();

    assert!(output.status.success());
    assert_eq!(
        report["selection"]["packages"],
        serde_json::json!(["alpha", "suite-docs"])
    );
    assert_eq!(report["selected_population"], 2);
    assert_eq!(report["coverage"]["denominator"], 2);
}

#[test]
fn package_filter_combines_with_install_reason() {
    let data_home = tempfile::tempdir().unwrap();
    let fixture_bin = fixture_bin();
    assert!(
        refresh(&["--format", "json"], data_home.path(), fixture_bin.path())
            .status
            .success()
    );

    // suite-cli is a dependency; combining it with --explicit selects nothing.
    let output = run_with_inventory(
        &[
            "count",
            "--explicit",
            "--package",
            "suite-cli",
            "--format",
            "json",
        ],
        data_home.path(),
        fixture_bin.path(),
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();

    assert!(output.status.success());
    assert_eq!(report["selected_population"], 0);
    assert_eq!(report["coverage"]["denominator"], 0);
}

#[test]
fn unmatched_package_is_an_explicit_successful_zero_report() {
    let data_home = tempfile::tempdir().unwrap();
    let fixture_bin = fixture_bin();
    assert!(
        refresh(&["--format", "json"], data_home.path(), fixture_bin.path())
            .status
            .success()
    );

    let output = run_with_inventory(
        &["count", "--package", "does-not-exist", "--format", "json"],
        data_home.path(),
        fixture_bin.path(),
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();

    assert!(output.status.success());
    assert_eq!(report["installed_total"], 4);
    assert_eq!(report["selected_population"], 0);
}

#[test]
fn invalid_package_names_exit_two() {
    for name in ["bad name", "bad/name", ""] {
        let data_home = tempfile::tempdir().unwrap();
        let output = run(
            &["count", "--format", "json", "--package", name],
            data_home.path(),
        );
        let error: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();

        assert_eq!(output.status.code(), Some(2), "name: {name:?}");
        assert_eq!(error["error"]["code"], "invalid_cli");
    }
}

#[test]
fn human_formats_quote_origins_that_resemble_filter_syntax() {
    let data_home = tempfile::tempdir().unwrap();
    let fixture_bin = fixture_bin();
    assert!(
        refresh(&["--format", "json"], data_home.path(), fixture_bin.path())
            .status
            .success()
    );
    let connection = Connection::open(data_home.path().join("paclang/paclang.db")).unwrap();
    connection
        .execute(
            "UPDATE installed_packages SET origin = 'all' WHERE name = 'alpha'",
            [],
        )
        .unwrap();
    connection
        .execute(
            "UPDATE installed_packages SET origin = 'core, extra' WHERE name = 'foreign-tool'",
            [],
        )
        .unwrap();

    for format in ["terminal", "markdown"] {
        let output = run_with_inventory(
            &[
                "count",
                "--origin",
                "core, extra",
                "--origin",
                "all",
                "--format",
                format,
            ],
            data_home.path(),
            fixture_bin.path(),
        );
        let stdout = String::from_utf8(output.stdout).unwrap();

        assert!(output.status.success(), "format: {format}");
        assert!(stdout.contains("origins = [\"all\", \"core, extra\"]"));
        assert!(stdout.contains("Selected population: 2"));
    }
}

#[test]
fn unmatched_origin_is_an_explicit_successful_zero_report() {
    let data_home = tempfile::tempdir().unwrap();
    let fixture_bin = fixture_bin();
    assert!(
        refresh(&["--format", "json"], data_home.path(), fixture_bin.path())
            .status
            .success()
    );

    let output = run_with_inventory(
        &["count", "--origin", "does-not-exist", "--format", "json"],
        data_home.path(),
        fixture_bin.path(),
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();

    assert!(output.status.success());
    assert_eq!(report["installed_total"], 4);
    assert_eq!(report["selected_population"], 0);
    assert_eq!(report["coverage"]["denominator"], 0);
    assert_eq!(report["coverage"]["missing_source_language_analysis"], 0);
}

#[test]
fn install_reason_filters_are_mutually_exclusive() {
    let data_home = tempfile::tempdir().unwrap();
    let output = run(
        &["count", "--explicit", "--dependency", "--format", "json"],
        data_home.path(),
    );
    let error: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();

    assert_eq!(output.status.code(), Some(2));
    assert_eq!(error["error"]["code"], "invalid_cli");
}

#[test]
fn filtered_report_facts_match_in_every_format() {
    let data_home = tempfile::tempdir().unwrap();
    let fixture_bin = fixture_bin();
    assert!(
        refresh(&["--format", "json"], data_home.path(), fixture_bin.path())
            .status
            .success()
    );
    assign_fixture_origins(data_home.path());

    for format in ["terminal", "toon", "json", "markdown"] {
        let output = run_with_inventory(
            &[
                "count",
                "--explicit",
                "--origin",
                "core",
                "--format",
                format,
            ],
            data_home.path(),
            fixture_bin.path(),
        );
        let stdout = String::from_utf8(output.stdout).unwrap();

        assert!(output.status.success(), "format: {format}");
        match format {
            "json" => {
                let report: serde_json::Value = serde_json::from_str(&stdout).unwrap();
                assert_eq!(report["selection"]["install_reason"], "explicit");
                assert_eq!(report["selection"]["origins"], serde_json::json!(["core"]));
                assert_eq!(report["selected_population"], 1);
                assert_eq!(report["coverage"]["denominator"], 1);
            }
            "toon" => {
                let report: serde_json::Value = toon_format::decode_default(&stdout).unwrap();
                assert_eq!(report["selection"]["install_reason"], "explicit");
                assert_eq!(report["selection"]["origins"], serde_json::json!(["core"]));
                assert_eq!(report["selected_population"], 1);
                assert_eq!(report["coverage"]["denominator"], 1);
            }
            "terminal" | "markdown" => {
                assert!(stdout.contains("install reason = explicit; origins = [\"core\"]"));
                assert!(stdout.contains("Selected population: 1"));
                assert!(stdout.contains("Coverage denominator: 1"));
            }
            _ => unreachable!(),
        }
    }
}

#[test]
fn unmet_coverage_threshold_exits_one_without_changing_report_facts() {
    let data_home = tempfile::tempdir().unwrap();
    let fixture_bin = fixture_bin();
    assert!(
        refresh(&["--format", "json"], data_home.path(), fixture_bin.path())
            .status
            .success()
    );

    let baseline = run_with_inventory(
        &["count", "--format", "json"],
        data_home.path(),
        fixture_bin.path(),
    );
    assert_eq!(baseline.status.code(), Some(0));

    // Metadata-only snapshots carry no analyzed packages, so any positive
    // threshold fails while leaving the report facts untouched.
    let gated = run_with_inventory(
        &["count", "--format", "json", "--min-coverage", "50"],
        data_home.path(),
        fixture_bin.path(),
    );
    assert_eq!(gated.status.code(), Some(1));
    assert!(gated.stderr.is_empty());
    assert_eq!(gated.stdout, baseline.stdout);

    let satisfied = run_with_inventory(
        &["count", "--format", "json", "--min-coverage", "0"],
        data_home.path(),
        fixture_bin.path(),
    );
    assert_eq!(satisfied.status.code(), Some(0));
    assert_eq!(satisfied.stdout, baseline.stdout);
}

#[test]
fn coverage_threshold_passes_for_an_empty_selection() {
    let data_home = tempfile::tempdir().unwrap();
    let fixture_bin = fixture_bin();
    assert!(
        refresh(&["--format", "json"], data_home.path(), fixture_bin.path())
            .status
            .success()
    );

    let output = run_with_inventory(
        &[
            "count",
            "--origin",
            "does-not-exist",
            "--format",
            "json",
            "--min-coverage",
            "100",
        ],
        data_home.path(),
        fixture_bin.path(),
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(report["selected_population"], 0);
}

#[test]
fn invalid_coverage_thresholds_exit_two() {
    for threshold in ["101", "-1", "not-a-number", "nan", "inf"] {
        let data_home = tempfile::tempdir().unwrap();
        let output = run(
            &["count", "--format", "json", "--min-coverage", threshold],
            data_home.path(),
        );
        let error: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();

        assert_eq!(output.status.code(), Some(2), "threshold: {threshold}");
        assert_eq!(error["error"]["code"], "invalid_cli");
    }
}

#[test]
fn refresh_with_an_unmet_threshold_still_publishes_and_exits_one() {
    let data_home = tempfile::tempdir().unwrap();
    let fixture_bin = fixture_bin();

    let output = refresh(
        &["--format", "json", "--min-coverage", "50"],
        data_home.path(),
        fixture_bin.path(),
    );

    assert_eq!(output.status.code(), Some(1));
    assert!(output.stderr.is_empty());
    assert_eq!(snapshot_count(data_home.path()), 1);
}

#[test]
fn invalid_metadata_does_not_replace_the_published_snapshot() {
    let data_home = tempfile::tempdir().unwrap();
    let fixture_bin = fixture_bin();
    let refreshed = refresh(&["--format", "json"], data_home.path(), fixture_bin.path());
    assert_eq!(refreshed.status.code(), Some(0));

    write_executable(
        &fixture_bin.path().join("expac"),
        "#!/bin/sh\nprintf 'alpha\\tbroken\\n'\n",
    );
    let failed = refresh(&["--format", "json"], data_home.path(), fixture_bin.path());
    let error: serde_json::Value = serde_json::from_slice(&failed.stdout).unwrap();
    assert_eq!(failed.status.code(), Some(1));
    assert_eq!(error["error"]["code"], "inventory");

    let count = run_with_inventory(
        &["count", "--format", "json"],
        data_home.path(),
        fixture_bin.path(),
    );
    let report: serde_json::Value = serde_json::from_slice(&count.stdout).unwrap();
    assert_eq!(count.status.code(), Some(0));
    assert_eq!(report["installed_total"], 4);
}

#[test]
fn failed_publication_rolls_back_the_whole_snapshot() {
    let data_home = tempfile::tempdir().unwrap();
    let fixture_bin = fixture_bin();
    assert!(
        refresh(&["--format", "json"], data_home.path(), fixture_bin.path())
            .status
            .success()
    );
    let connection = Connection::open(data_home.path().join("paclang/paclang.db")).unwrap();
    connection
        .execute_batch(
            "CREATE TRIGGER reject_alpha
             BEFORE INSERT ON installed_packages
             WHEN NEW.name = 'alpha'
             BEGIN
               SELECT RAISE(ABORT, 'test publication failure');
             END;",
        )
        .unwrap();
    drop(connection);

    let failed = refresh(&["--format", "json"], data_home.path(), fixture_bin.path());
    let error: serde_json::Value = serde_json::from_slice(&failed.stdout).unwrap();
    assert_eq!(failed.status.code(), Some(1));
    assert_eq!(error["error"]["code"], "storage");
    assert_eq!(snapshot_count(data_home.path()), 1);

    let count = run_with_inventory(
        &["count", "--format", "json"],
        data_home.path(),
        fixture_bin.path(),
    );
    let report: serde_json::Value = serde_json::from_slice(&count.stdout).unwrap();
    assert!(count.status.success());
    assert_eq!(report["installed_total"], 4);
}

#[test]
fn interrupted_refresh_preserves_the_previous_snapshot_and_exits_130() {
    let data_home = tempfile::tempdir().unwrap();
    let fixture_bin = fixture_bin();
    assert!(
        refresh(&["--format", "json"], data_home.path(), fixture_bin.path())
            .status
            .success()
    );
    let marker = data_home.path().join("expac-started");
    let gate = data_home.path().join("expac-gate");
    assert!(
        Command::new("mkfifo")
            .arg(&gate)
            .status()
            .unwrap()
            .success()
    );
    write_executable(
        &fixture_bin.path().join("expac"),
        "#!/bin/sh\n\
         /bin/sleep 30 &\n\
         : > \"$PACLANG_STARTED\"\n\
         IFS= read -r _ < \"$PACLANG_GATE\"\n",
    );
    let child = command(data_home.path())
        .args(["refresh", "--metadata-only", "--format", "json"])
        .env("PATH", fixture_bin.path())
        .env("PACLANG_STARTED", &marker)
        .env("PACLANG_GATE", &gate)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    wait_for_file(&marker);
    let interrupted_at = Instant::now();
    assert!(
        Command::new("kill")
            .args(["-INT", &child.id().to_string()])
            .status()
            .unwrap()
            .success()
    );
    let output = child.wait_with_output().unwrap();
    let error: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();

    assert_eq!(output.status.code(), Some(130));
    assert!(interrupted_at.elapsed() < Duration::from_secs(1));
    assert_eq!(error["error"]["code"], "interrupted");
    assert_eq!(snapshot_count(data_home.path()), 1);

    let count = run_with_inventory(
        &["count", "--format", "json"],
        data_home.path(),
        fixture_bin.path(),
    );
    assert!(count.status.success());
}

#[test]
fn concurrent_refresh_cannot_enter_the_writer_transaction() {
    let data_home = tempfile::tempdir().unwrap();
    let fixture_bin = fixture_bin();
    assert!(
        refresh(&["--format", "json"], data_home.path(), fixture_bin.path())
            .status
            .success()
    );
    let marker = data_home.path().join("writer-started");
    let gate = data_home.path().join("writer-gate");
    assert!(
        Command::new("mkfifo")
            .arg(&gate)
            .status()
            .unwrap()
            .success()
    );
    write_executable(
        &fixture_bin.path().join("expac"),
        "#!/bin/sh\n\
         : > \"$PACLANG_STARTED\"\n\
         IFS= read -r _ < \"$PACLANG_GATE\"\n",
    );
    let first = command(data_home.path())
        .args(["refresh", "--metadata-only", "--format", "json"])
        .env("PATH", fixture_bin.path())
        .env("PACLANG_STARTED", &marker)
        .env("PACLANG_GATE", &gate)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    wait_for_file(&marker);

    let count = run_with_inventory(
        &["count", "--format", "json"],
        data_home.path(),
        fixture_bin.path(),
    );
    let report: serde_json::Value = serde_json::from_slice(&count.stdout).unwrap();
    assert!(count.status.success());
    assert_eq!(report["installed_total"], 4);

    let second = refresh(&["--format", "json"], data_home.path(), fixture_bin.path());
    let error: serde_json::Value = serde_json::from_slice(&second.stdout).unwrap();
    assert_eq!(second.status.code(), Some(1));
    assert_eq!(error["error"]["code"], "storage");
    assert_eq!(snapshot_count(data_home.path()), 1);

    assert!(
        Command::new("kill")
            .args(["-INT", &first.id().to_string()])
            .status()
            .unwrap()
            .success()
    );
    assert_eq!(first.wait_with_output().unwrap().status.code(), Some(130));
}

#[test]
fn interrupted_writer_lock_wait_exits_130_promptly() {
    let data_home = tempfile::tempdir().unwrap();
    let fixture_bin = fixture_bin();
    assert!(
        refresh(&["--format", "json"], data_home.path(), fixture_bin.path())
            .status
            .success()
    );
    let marker = data_home.path().join("lock-holder-started");
    let gate = data_home.path().join("lock-holder-gate");
    assert!(
        Command::new("mkfifo")
            .arg(&gate)
            .status()
            .unwrap()
            .success()
    );
    write_executable(
        &fixture_bin.path().join("expac"),
        "#!/bin/sh\n\
         : > \"$PACLANG_STARTED\"\n\
         IFS= read -r _ < \"$PACLANG_GATE\"\n",
    );
    let holder = command(data_home.path())
        .args(["refresh", "--metadata-only", "--format", "json"])
        .env("PATH", fixture_bin.path())
        .env("PACLANG_STARTED", &marker)
        .env("PACLANG_GATE", &gate)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    wait_for_file(&marker);

    let waiter = command(data_home.path())
        .args(["refresh", "--metadata-only", "--format", "json"])
        .env("PATH", fixture_bin.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(100));
    let interrupted_at = Instant::now();
    assert!(
        Command::new("kill")
            .args(["-INT", &waiter.id().to_string()])
            .status()
            .unwrap()
            .success()
    );
    let output = waiter.wait_with_output().unwrap();
    let error: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(interrupted_at.elapsed() < Duration::from_secs(1));
    assert_eq!(output.status.code(), Some(130));
    assert_eq!(error["error"]["code"], "interrupted");

    assert!(
        Command::new("kill")
            .args(["-INT", &holder.id().to_string()])
            .status()
            .unwrap()
            .success()
    );
    assert_eq!(holder.wait_with_output().unwrap().status.code(), Some(130));
    assert_eq!(snapshot_count(data_home.path()), 1);
}

#[test]
fn retention_waits_until_an_overlapping_report_finishes() {
    let data_home = tempfile::tempdir().unwrap();
    let baseline_bin = fixture_bin();
    for _ in 0..10 {
        assert!(
            refresh(&["--format", "json"], data_home.path(), baseline_bin.path())
                .status
                .success()
        );
    }

    let report_bin = fixture_bin();
    let report_started = data_home.path().join("report-started");
    let report_gate = data_home.path().join("report-gate");
    assert!(
        Command::new("mkfifo")
            .arg(&report_gate)
            .status()
            .unwrap()
            .success()
    );
    write_executable(
        &report_bin.path().join("pacman"),
        "#!/bin/sh\n\
         [ \"$1\" = '-Q' ] || exit 64\n\
         : > \"$PACLANG_STARTED\"\n\
         IFS= read -r _ < \"$PACLANG_GATE\"\n\
         printf 'alpha 1.0-1\\nforeign-tool 3.0-1\\nsuite-cli 2.0-1\\nsuite-docs 2.0-1\\n'\n",
    );
    let report = command(data_home.path())
        .args(["count", "--format", "json"])
        .env("PATH", report_bin.path())
        .env("PACLANG_STARTED", &report_started)
        .env("PACLANG_GATE", &report_gate)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    wait_for_file(&report_started);

    let refresh_bin = fixture_bin();
    let capture_state = data_home.path().join("capture-state");
    let capture_done = data_home.path().join("capture-done");
    std::fs::write(&capture_state, "0\n").unwrap();
    write_executable(
        &refresh_bin.path().join("pacman"),
        "#!/bin/sh\n\
         [ \"$#\" -eq 1 ] || exit 64\n\
         if [ \"$1\" = '-Qq' ]; then\n\
           printf 'alpha\\nforeign-tool\\nsuite-cli\\nsuite-docs\\n'\n\
         elif [ \"$1\" = '-Q' ]; then\n\
           IFS= read -r count < \"$PACLANG_STATE\"\n\
           count=$((count + 1))\n\
           printf '%s\\n' \"$count\" > \"$PACLANG_STATE\"\n\
           [ \"$count\" -ne 2 ] || : > \"$PACLANG_CAPTURED\"\n\
           printf 'alpha 1.0-1\\nforeign-tool 3.0-1\\nsuite-cli 2.0-1\\nsuite-docs 2.0-1\\n'\n\
         else\n\
           exit 64\n\
         fi\n",
    );
    let mut refreshing = command(data_home.path())
        .args(["refresh", "--metadata-only", "--format", "json"])
        .env("PATH", refresh_bin.path())
        .env("PACLANG_STATE", &capture_state)
        .env("PACLANG_CAPTURED", &capture_done)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    wait_for_file(&capture_done);
    std::thread::sleep(Duration::from_millis(100));
    assert!(refreshing.try_wait().unwrap().is_none());
    assert_eq!(snapshot_count(data_home.path()), 10);

    OpenOptions::new()
        .write(true)
        .open(&report_gate)
        .unwrap()
        .write_all(b"continue\n")
        .unwrap();
    assert!(report.wait_with_output().unwrap().status.success());
    assert!(refreshing.wait_with_output().unwrap().status.success());
    assert_eq!(snapshot_count(data_home.path()), 10);
}

#[test]
fn refresh_retains_only_the_latest_ten_published_snapshots() {
    let data_home = tempfile::tempdir().unwrap();
    let fixture_bin = fixture_bin();

    for _ in 0..12 {
        let output = refresh(&["--format", "json"], data_home.path(), fixture_bin.path());
        assert!(output.status.success());
    }

    assert_eq!(snapshot_count(data_home.path()), 10);
    let connection = Connection::open(data_home.path().join("paclang/paclang.db")).unwrap();
    let orphaned_packages: u32 = connection
        .query_row(
            "SELECT count(*) FROM installed_packages
             WHERE snapshot_id NOT IN (SELECT id FROM inventory_snapshots)",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(orphaned_packages, 0);
}

#[test]
fn refresh_publishes_unknown_origins_without_inference() {
    let data_home = tempfile::tempdir().unwrap();
    let fixture_bin = fixture_bin();
    let output = refresh(&["--format", "json"], data_home.path(), fixture_bin.path());
    assert_eq!(output.status.code(), Some(0));

    let connection = Connection::open(data_home.path().join("paclang/paclang.db")).unwrap();
    let mut statement = connection
        .prepare(
            "SELECT name, package_base, origin FROM installed_packages
             ORDER BY name",
        )
        .unwrap();
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();

    assert_eq!(
        rows,
        [
            (
                "alpha".to_owned(),
                Some("alpha".to_owned()),
                "unknown".to_owned()
            ),
            (
                "foreign-tool".to_owned(),
                Some("foreign-tool".to_owned()),
                "unknown".to_owned()
            ),
            (
                "suite-cli".to_owned(),
                Some("suite".to_owned()),
                "unknown".to_owned()
            ),
            (
                "suite-docs".to_owned(),
                Some("suite".to_owned()),
                "unknown".to_owned()
            ),
        ]
    );
}

#[test]
fn version_one_snapshot_marker_migrates_to_no_snapshot() {
    let data_home = tempfile::tempdir().unwrap();
    let database_directory = data_home.path().join("paclang");
    std::fs::create_dir(&database_directory).unwrap();
    let database_path = database_directory.join("paclang.db");
    let connection = Connection::open(&database_path).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE inventory_snapshots (
                id INTEGER PRIMARY KEY,
                published_at TEXT NOT NULL
             ) STRICT;
             INSERT INTO inventory_snapshots (published_at) VALUES ('legacy-marker');
             PRAGMA user_version = 1;",
        )
        .unwrap();
    drop(connection);

    let output = run(&["count", "--format", "json"], data_home.path());
    let error: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(error["error"]["code"], "no_snapshot");

    let connection = Connection::open(database_path).unwrap();
    let version: u32 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    let snapshots: u32 = connection
        .query_row("SELECT count(*) FROM inventory_snapshots", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(version, 11);
    assert_eq!(snapshots, 0);
}

#[test]
fn version_two_snapshot_migrates_and_reports_unknown_staleness() {
    let data_home = tempfile::tempdir().unwrap();
    let fixture_bin = fixture_bin();
    let database_directory = data_home.path().join("paclang");
    std::fs::create_dir(&database_directory).unwrap();
    let database_path = database_directory.join("paclang.db");
    let connection = Connection::open(&database_path).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE inventory_snapshots (
                id INTEGER PRIMARY KEY,
                published_at TEXT NOT NULL
             ) STRICT;
             CREATE TABLE installed_packages (
                snapshot_id INTEGER NOT NULL REFERENCES inventory_snapshots(id) ON DELETE CASCADE,
                name TEXT NOT NULL,
                version TEXT NOT NULL,
                package_base TEXT,
                origin TEXT NOT NULL,
                install_reason TEXT NOT NULL CHECK (install_reason IN ('explicit', 'dependency')),
                installed_bytes INTEGER NOT NULL CHECK (installed_bytes >= 0),
                PRIMARY KEY (snapshot_id, name)
             ) STRICT;
             INSERT INTO inventory_snapshots (published_at) VALUES ('old-snapshot');
             INSERT INTO installed_packages (
                snapshot_id, name, version, package_base, origin, install_reason, installed_bytes
             ) VALUES
                (1, 'alpha', '1.0-1', 'alpha', 'unknown', 'explicit', 120),
                (1, 'foreign-tool', '3.0-1', 'foreign-tool', 'unknown', 'explicit', 40),
                (1, 'suite-cli', '2.0-1', 'suite', 'unknown', 'dependency', 300),
                (1, 'suite-docs', '2.0-1', 'suite', 'unknown', 'dependency', 80);
             PRAGMA user_version = 2;",
        )
        .unwrap();
    drop(connection);

    let output = run_with_inventory(
        &["count", "--format", "json"],
        data_home.path(),
        fixture_bin.path(),
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(output.status.success());
    assert_eq!(report["snapshot_staleness"], "unknown");
    assert_eq!(report["installed_total"], 4);

    let connection = Connection::open(database_path).unwrap();
    let version: u32 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(version, 11);
}

fn bin_with_only(tool: &str, source_bin: &Path) -> tempfile::TempDir {
    let directory = tempfile::tempdir().unwrap();
    std::fs::copy(source_bin.join(tool), directory.path().join(tool)).unwrap();
    directory
}

fn write_expac(directory: &Path, script_body: &str) {
    write_executable(&directory.join("expac"), script_body);
}

#[test]
fn missing_pacman_produces_a_typed_adapter_error() {
    let data_home = tempfile::tempdir().unwrap();
    let fixture_bin = fixture_bin();
    let only_expac = bin_with_only("expac", fixture_bin.path());

    let output = command(data_home.path())
        .args(["refresh", "--metadata-only", "--format", "json"])
        .env("PATH", only_expac.path())
        .output()
        .unwrap();
    let error: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();

    assert_eq!(output.status.code(), Some(1));
    assert!(output.stderr.is_empty());
    assert_eq!(error["error"]["code"], "inventory");
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("cannot start pacman"),
        "message: {}",
        error["error"]["message"]
    );
}

#[test]
fn missing_expac_produces_a_typed_adapter_error() {
    let data_home = tempfile::tempdir().unwrap();
    let fixture_bin = fixture_bin();
    let only_pacman = bin_with_only("pacman", fixture_bin.path());

    let output = command(data_home.path())
        .args(["refresh", "--metadata-only", "--format", "json"])
        .env("PATH", only_pacman.path())
        .output()
        .unwrap();
    let error: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();

    assert_eq!(output.status.code(), Some(1));
    assert!(output.stderr.is_empty());
    assert_eq!(error["error"]["code"], "inventory");
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("cannot start expac"),
        "message: {}",
        error["error"]["message"]
    );
}

#[test]
fn nonzero_adapter_exit_keeps_diagnostic_context_and_the_prior_snapshot() {
    let data_home = tempfile::tempdir().unwrap();
    let fixture_bin = fixture_bin();
    assert!(
        refresh(&["--format", "json"], data_home.path(), fixture_bin.path())
            .status
            .success()
    );
    write_expac(
        fixture_bin.path(),
        "#!/bin/sh\n\
         echo 'expac database is locked' >&2\n\
         exit 1\n",
    );

    let failed = refresh(&["--format", "json"], data_home.path(), fixture_bin.path());
    let error: serde_json::Value = serde_json::from_slice(&failed.stdout).unwrap();

    assert_eq!(failed.status.code(), Some(1));
    assert_eq!(error["error"]["code"], "inventory");
    let message = error["error"]["message"].as_str().unwrap();
    assert!(message.contains("expac"), "message: {message}");
    assert!(
        message.contains("expac database is locked"),
        "message: {message}"
    );
    assert_eq!(snapshot_count(data_home.path()), 1);

    // Restore the working adapter for the follow-up report.
    std::fs::remove_file(fixture_bin.path().join("expac")).unwrap();
    write_executable(
        &fixture_bin.path().join("expac"),
        "#!/bin/sh\n\
         [ \"$#\" -eq 2 ] && [ \"$1\" = '-Q' ] || exit 64\n\
         printf 'alpha\\t1.0-1\\talpha\\tlocal\\texplicit\\t120\\n\
foreign-tool\\t3.0-1\\tforeign-tool\\tlocal\\texplicit\\t40\\n\
suite-cli\\t2.0-1\\tsuite\\tlocal\\tdependency\\t300\\n\
suite-docs\\t2.0-1\\tsuite\\tlocal\\tdependency\\t80\\n'\n",
    );
    let count = run_with_inventory(
        &["count", "--format", "json"],
        data_home.path(),
        fixture_bin.path(),
    );
    let report: serde_json::Value = serde_json::from_slice(&count.stdout).unwrap();
    assert!(count.status.success());
    assert_eq!(report["installed_total"], 4);
}

#[test]
fn unbounded_adapter_stderr_is_truncated_not_leaked() {
    let data_home = tempfile::tempdir().unwrap();
    let fixture_bin = fixture_bin();
    assert!(
        refresh(&["--format", "json"], data_home.path(), fixture_bin.path())
            .status
            .success()
    );
    write_expac(
        fixture_bin.path(),
        "#!/bin/sh\n\
         pad='xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx'\n\
         chunk=\"$pad$pad$pad$pad$pad$pad$pad$pad\"\n\
         chunk=\"$chunk$chunk$chunk$chunk\"\n\
         while [ \"${#chunk}\" -lt 102400 ]; do chunk=\"$chunk$chunk\"; done\n\
         printf '%s' \"$chunk\" >&2\n\
         exit 1\n",
    );

    let failed = refresh(&["--format", "json"], data_home.path(), fixture_bin.path());
    let error: serde_json::Value = serde_json::from_slice(&failed.stdout).unwrap();
    let message = error["error"]["message"].as_str().unwrap();

    assert_eq!(failed.status.code(), Some(1));
    assert_eq!(error["error"]["code"], "inventory");
    assert!(message.contains("[truncated]"), "message tail: {message}");
    assert!(
        message.len() < 8 * 1024,
        "diagnostic context must stay bounded, got {} bytes",
        message.len()
    );
    assert_eq!(snapshot_count(data_home.path()), 1);
}

#[test]
fn invalid_utf8_adapter_output_cannot_publish() {
    let data_home = tempfile::tempdir().unwrap();
    let fixture_bin = fixture_bin();
    assert!(
        refresh(&["--format", "json"], data_home.path(), fixture_bin.path())
            .status
            .success()
    );
    write_expac(
        fixture_bin.path(),
        "#!/bin/sh\nprintf '\\xff\\xfe\\x00broken\\n'\n",
    );

    let failed = refresh(&["--format", "json"], data_home.path(), fixture_bin.path());
    let error: serde_json::Value = serde_json::from_slice(&failed.stdout).unwrap();

    assert_eq!(failed.status.code(), Some(1));
    assert_eq!(error["error"]["code"], "inventory");
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("not UTF-8"),
        "message: {}",
        error["error"]["message"]
    );
    assert_eq!(snapshot_count(data_home.path()), 1);
}

#[test]
fn malformed_metadata_variants_cannot_replace_the_published_snapshot() {
    // Each case replaces the fixture `expac` with output that must be
    // rejected: malformed rows, invalid names, invalid sizes, duplicate
    // rows, and conflicting rows for one package name.
    let cases: &[(&str, &str)] = &[
        ("malformed row", "alpha\tbroken\n"),
        (
            "invalid name with whitespace",
            "bad name\t1.0-1\tbad name\tlocal\texplicit\t10\n",
        ),
        (
            "invalid name with slash",
            "bad/name\t1.0-1\tbad/name\tlocal\texplicit\t10\n",
        ),
        (
            "non-numeric size",
            "alpha\t1.0-1\talpha\tlocal\texplicit\tlots\n",
        ),
        (
            "negative size",
            "alpha\t1.0-1\talpha\tlocal\texplicit\t-5\n",
        ),
        (
            "overflowing size",
            "alpha\t1.0-1\talpha\tlocal\texplicit\t18446744073709551616\n",
        ),
        (
            "size exceeding sqlite range",
            "alpha\t1.0-1\talpha\tlocal\texplicit\t9223372036854775808\n",
        ),
        (
            "duplicate rows",
            "alpha\t1.0-1\talpha\tlocal\texplicit\t120\n\
             alpha\t1.0-1\talpha\tlocal\texplicit\t120\n",
        ),
        (
            "conflicting rows",
            "alpha\t1.0-1\talpha\tlocal\texplicit\t120\n\
             alpha\t2.0-1\talpha\tlocal\tdependency\t999\n",
        ),
        (
            "unexpected install reason",
            "alpha\t1.0-1\talpha\tlocal\tsomeday\t120\n",
        ),
        (
            "non-local queried database",
            "alpha\t1.0-1\talpha\textra\texplicit\t120\n",
        ),
    ];

    for (label, body) in cases {
        let data_home = tempfile::tempdir().unwrap();
        let fixture_bin = fixture_bin();
        assert!(
            refresh(&["--format", "json"], data_home.path(), fixture_bin.path())
                .status
                .success(),
            "setup failed for case: {label}"
        );
        let escaped = body.replace('\\', "\\\\").replace('\'', "'\\''");
        write_expac(
            fixture_bin.path(),
            &format!("#!/bin/sh\nprintf '{escaped}'\n"),
        );

        let failed = refresh(&["--format", "json"], data_home.path(), fixture_bin.path());
        let error: serde_json::Value = serde_json::from_slice(&failed.stdout).unwrap();
        assert_eq!(failed.status.code(), Some(1), "case: {label}");
        assert_eq!(error["error"]["code"], "inventory", "case: {label}");
        assert_eq!(snapshot_count(data_home.path()), 1, "case: {label}");

        let count = run_with_inventory(
            &["count", "--format", "json"],
            data_home.path(),
            fixture_bin.path(),
        );
        // The follow-up report still reads live inventory through the broken
        // expac fixture for nothing: `count` only needs `pacman -Q`, so it
        // succeeds against the preserved snapshot.
        let report: serde_json::Value = serde_json::from_slice(&count.stdout).unwrap();
        assert!(count.status.success(), "case: {label}");
        assert_eq!(report["installed_total"], 4, "case: {label}");
    }
}

#[test]
fn oversized_adapter_output_is_rejected_without_publishing() {
    let data_home = tempfile::tempdir().unwrap();
    let fixture_bin = fixture_bin();
    assert!(
        refresh(&["--format", "json"], data_home.path(), fixture_bin.path())
            .status
            .success()
    );
    write_expac(
        fixture_bin.path(),
        "#!/bin/sh\n\
         pad='xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx'\n\
         chunk=\"$pad$pad$pad$pad$pad$pad$pad$pad\"\n\
         chunk=\"$chunk$chunk$chunk$chunk\"\n\
         while [ \"${#chunk}\" -le 16777216 ]; do chunk=\"$chunk$chunk\"; done\n\
         printf '%s' \"$chunk\"\n",
    );

    let failed = refresh(&["--format", "json"], data_home.path(), fixture_bin.path());
    let error: serde_json::Value = serde_json::from_slice(&failed.stdout).unwrap();

    assert_eq!(failed.status.code(), Some(1));
    assert_eq!(error["error"]["code"], "inventory");
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("capture limit"),
        "message: {}",
        error["error"]["message"]
    );
    assert_eq!(snapshot_count(data_home.path()), 1);
}
