use std::process::Command;

#[test]
fn version_flag_reports_package_version() {
    let output = Command::new(env!("CARGO_BIN_EXE_branchfs"))
        .arg("--version")
        .output()
        .expect("run branchfs --version");

    assert!(
        output.status.success(),
        "branchfs --version failed: status={:?} stderr={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(env!("CARGO_PKG_VERSION")),
        "version output {stdout:?} did not contain {}",
        env!("CARGO_PKG_VERSION")
    );
}
