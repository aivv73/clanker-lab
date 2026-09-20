#[test]
fn real_controller_policy_cancellation_eof_and_restart() {
    let result = std::process::Command::new("python3")
        .arg(format!(
            "{}/tests/fixtures/policy.py",
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
