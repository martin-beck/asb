//! Explicit local PTY coverage for setup TUI Left/Right navigation.
#![cfg(target_os = "linux")]
mod support;

use std::process::Command;
use support::IsolatedEnvironment;

#[test]
#[ignore = "local-only TUI PTY stress; run cargo test --test tui_arrow_stress -- --ignored"]
fn local_arrow_navigation_generates_and_cancels_without_publishing_partial_files() {
    let output = Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/tui_terminal.py"
        ))
        .arg(env!("CARGO_BIN_EXE_asb"))
        .isolated_env()
        .env("ASB_TUI_LOCAL_STRESS", "1")
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
