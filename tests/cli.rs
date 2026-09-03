use std::fs::{File, OpenOptions};
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output, Stdio};

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
         [ \"$#\" -eq 1 ] && [ \"$1\" = '-Qq' ] || exit 64\n\
         printf 'alpha\\nforeign-tool\\nsuite-cli\\nsuite-docs\\n'\n",
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
    assert_eq!(version, 2);
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
                assert_eq!(value["schema_version"], "1.1.0");
                assert_eq!(value["error"]["code"], "invalid_cli");
            }
            "markdown" => assert!(stdout.contains("- Code: `invalid_cli`")),
            "toon" => {
                let value: serde_json::Value = toon_format::decode_default(&stdout).unwrap();
                assert_eq!(value["schema_version"], "1.1.0");
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
fn count_reads_the_snapshot_without_running_inventory_commands() {
    let data_home = tempfile::tempdir().unwrap();
    let fixture_bin = fixture_bin();
    let refreshed = refresh(&["--format", "json"], data_home.path(), fixture_bin.path());
    assert_eq!(refreshed.status.code(), Some(0));

    let empty_path = tempfile::tempdir().unwrap();
    let output = command(data_home.path())
        .args(["count", "--format", "json"])
        .env("PATH", empty_path.path())
        .output()
        .unwrap();
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(value["installed_total"], 4);
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
        let output = run(&["count", filter, "--format", "json"], data_home.path());
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

    let output = run(
        &[
            "count", "--origin", "extra", "--origin", "core", "--format", "json",
        ],
        data_home.path(),
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
        let output = run(
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

    let output = run(
        &["count", "--origin", "does-not-exist", "--format", "json"],
        data_home.path(),
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
        let output = run(
            &[
                "count",
                "--explicit",
                "--origin",
                "core",
                "--format",
                format,
            ],
            data_home.path(),
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

    let count = run(&["count", "--format", "json"], data_home.path());
    let report: serde_json::Value = serde_json::from_slice(&count.stdout).unwrap();
    assert_eq!(count.status.code(), Some(0));
    assert_eq!(report["installed_total"], 4);
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
    assert_eq!(version, 2);
    assert_eq!(snapshots, 0);
}
