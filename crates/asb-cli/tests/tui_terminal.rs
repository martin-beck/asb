//! Exercise actual terminal event loops without a real agent or provider.
#![cfg(target_os = "linux")]
mod support;
use std::process::Command;
use support::IsolatedEnvironment;

#[test]
fn offline_tui_terminal_navigation_restores_terminal_and_generates_configurations() {
    let output = Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/tui_terminal.py"
        ))
        .arg(env!("CARGO_BIN_EXE_asb"))
        .isolated_env()
        // Locate the Python harness here; its PTY children use their own
        // tool-free or explicitly stubbed PATH, never the host collectors.
        .env("PATH", "/usr/bin:/bin")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
