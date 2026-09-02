use std::fs::{File, OpenOptions};
use std::io::Read;
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
    assert_eq!(version, 1);
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
                assert_eq!(value["schema_version"], "1.0.0");
                assert_eq!(value["error"]["code"], "invalid_cli");
            }
            "markdown" => assert!(stdout.contains("- Code: `invalid_cli`")),
            "toon" => {
                let value: serde_json::Value = toon_format::decode_default(&stdout).unwrap();
                assert_eq!(value["schema_version"], "1.0.0");
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
