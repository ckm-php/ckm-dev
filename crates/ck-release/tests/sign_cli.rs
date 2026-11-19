//! The built `ck-release` binary: `sign` and `key check` refuse to run in CI
//! before they read a key (T2), and `key check` reads a key from stdin.

use std::env;
use std::process::{Command, Stdio};

/// The binary with neither CI variable set, run in the temp folder.
fn ck_release() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ck-release"));
    command
        .current_dir(env::temp_dir())
        .env_remove("CI")
        .env_remove("GITHUB_ACTIONS");
    command
}

// T2
#[test]
fn sign_and_key_check_refuse_in_ci_before_reading_the_key() {
    let sign: &[&str] = &[
        "sign",
        "--payload",
        "ck-release-test-payload.json",
        "--key",
        "-",
        "--out",
        "ck-release-test-catalog.json",
        "--first",
    ];
    let key_check: &[&str] = &["key", "check", "--key", "-"];
    for (variable, args) in [("CI", sign), ("GITHUB_ACTIONS", key_check)] {
        let output = ck_release()
            .args(args)
            .env(variable, "1")
            .stdin(Stdio::null())
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(1), "{variable}: {stderr}");
        // Missing files fail with exit code 1 too, so the message is checked.
        assert!(
            stderr.contains(&format!("{variable} is set")) && stderr.contains("offline only"),
            "{variable}: {stderr}"
        );
    }
}

#[cfg(unix)]
#[test]
fn key_check_reads_the_key_from_stdin() {
    use std::io::Write as _;

    // RFC 8032, section 7.1, test 1, as `openssl pkey` prints it.
    const PEM: &str = "-----BEGIN PRIVATE KEY-----\nMC4CAQAwBQYDK2VwBCIEIJ1hsZ3v/VpguoRK9JLsLMREScVpezJpGXA7rAMcrn9g\n-----END PRIVATE KEY-----\n";

    let mut child = ck_release()
        .args(["key", "check", "--key", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(PEM.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "public_key\td75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a\n\
         key_id\t21fe31dfa154a261\n\
         trust\tnot compiled in\n"
    );
}
