use std::process::Command;

#[test]
fn version_flag_prints_command_and_version() {
    let output = Command::new(env!("CARGO_BIN_EXE_ck"))
        .arg("--version")
        .output()
        .expect("run ck --version");

    assert!(output.status.success(), "ck --version failed: {output:?}");
    let stdout = String::from_utf8(output.stdout).expect("stdout is UTF-8");
    assert_eq!(
        stdout.trim_end(),
        format!("ck {}", env!("CARGO_PKG_VERSION"))
    );
}
