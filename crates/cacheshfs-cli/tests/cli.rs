//! End-to-end tests that run the built `cacheshfs` binary.
//!
//! These only assert on platform-neutral behavior — argument parsing, help,
//! and pre-dispatch validation errors — so they pass regardless of which mount
//! backend is compiled in or whether a filesystem driver is installed. Anything
//! that reaches a `MountBackend` is intentionally out of scope here.

use std::process::Command;

/// Path to the freshly built binary, provided by Cargo for integration tests.
fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_cacheshfs"))
}

#[test]
fn help_succeeds_and_lists_options() {
    let output = bin().arg("--help").output().expect("failed to run binary");
    assert!(output.status.success(), "exit: {:?}", output.status);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("--cache-mode"));
    assert!(stdout.contains("MOUNTPOINT"));
}

#[test]
fn version_succeeds() {
    let output = bin()
        .arg("--version")
        .output()
        .expect("failed to run binary");
    assert!(output.status.success());
}

#[test]
fn invalid_remote_fails_with_message() {
    let output = bin()
        .args(["badremote", "somewhere"])
        .output()
        .expect("failed to run binary");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("invalid remote"),
        "stderr was: {stderr}"
    );
}

#[test]
fn cache_dir_inside_mountpoint_fails() {
    let output = bin()
        .args([
            "host:/srv",
            "/mnt/remote",
            "--cache-dir",
            "/mnt/remote/cache",
        ])
        .output()
        .expect("failed to run binary");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("must not be inside the mountpoint"),
        "stderr was: {stderr}"
    );
}

#[test]
fn missing_arguments_fail() {
    let output = bin().output().expect("failed to run binary");
    assert!(!output.status.success());
}

#[test]
fn unknown_flag_fails() {
    let output = bin()
        .args(["host:/srv", "/mnt", "--definitely-not-a-flag"])
        .output()
        .expect("failed to run binary");
    assert!(!output.status.success());
}
