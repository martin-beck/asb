//! Offline tests for Cline's adapter-owned native tool hooks.
#![cfg(unix)]
mod support;
use support::IsolatedEnvironment;

#[test]
fn native_hooks_preserve_delegation_and_replay_tool_results() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let output = std::process::Command::new("node")
        .arg(root.join("tests/cline_native_hooks.mjs"))
        .current_dir(root)
        .isolated_env()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .output()
        .expect("Node is required for Cline adapter tests");
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
