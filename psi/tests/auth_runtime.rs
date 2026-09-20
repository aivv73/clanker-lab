#[test]
fn credentials_and_cancellation_across_real_runtime_processes() {
    let output = std::process::Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/runtime_auth.py"
        ))
        .arg(env!("CARGO_BIN_EXE_psi"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let evidence: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(evidence["passed"], true);
}
