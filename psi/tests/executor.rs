#[test]
fn real_isolation_resources_and_cleanup() {
    let _guard = EXECUTOR_TEST_LOCK.lock().unwrap();
    let result = std::process::Command::new("python3")
        .arg(format!(
            "{}/tests/fixtures/executor.py",
            env!("CARGO_MANIFEST_DIR")
        ))
        .arg(env!("CARGO_BIN_EXE_psi"))
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
}

#[test]
fn running_command_continuations() {
    // The integration suites acquire the same per-user lease; serialize these tests.
    let _guard = EXECUTOR_TEST_LOCK.lock().unwrap();
    let result = std::process::Command::new("python3")
        .arg(format!(
            "{}/tests/fixtures/continuation.py",
            env!("CARGO_MANIFEST_DIR")
        ))
        .arg(env!("CARGO_BIN_EXE_psi"))
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
}

static EXECUTOR_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
