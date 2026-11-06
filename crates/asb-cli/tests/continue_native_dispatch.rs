//! Offline prototype tests for Continue's native dispatch transport.
#![cfg(unix)]
mod support;
use support::IsolatedEnvironment;

#[test]
fn native_dispatch_prototype_handles_loader_ordering_and_child_transport() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    for fixture in [
        "continue_dispatch_transform.mjs",
        "continue_native_session.mjs",
        "continue_native_broker.mjs",
        "claude_native_hook.mjs",
        "claude_native_wire.mjs",
        "claude_count_probe.mjs",
        "claude_native_service.mjs",
        "claude_webfetch_tape.mjs",
        "claude_webfetch_proxy.mjs",
        "claude_webfetch_certificate.mjs",
        "claude_webfetch_service.mjs",
        "claude_native_launcher.mjs",
        "continue_native_sidecar.mjs",
        "continue_native_supervisor.mjs",
        "continue_native_service.mjs",
        "pi_native_transform.mjs",
        "pi_native_integration.mjs",
        "pi_native_identity.mjs",
    ] {
        let output = std::process::Command::new("node")
            .arg(root.join("tests").join(fixture))
            .current_dir(root)
            .isolated_env()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .output()
            .expect("Node is required for Continue adapter tests");
        assert!(
            output.status.success(),
            "fixture={fixture} stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
#[ignore = "requires ASB_TEST_PI_BUNDLE and unprivileged Linux network namespaces"]
fn installed_pi_preserves_snapshot_and_result_order_offline() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let bundle = std::env::var_os("ASB_TEST_PI_BUNDLE").expect("installed Pi bundle is required");
    let output = std::process::Command::new("node")
        .arg(root.join("tests/pi_native_installed_probe.mjs"))
        .current_dir(root)
        .isolated_env()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("ASB_TEST_PI_BUNDLE", bundle)
        .output()
        .expect("Node is required for the installed Pi probe");
    assert!(
        output.status.success(),
        "installed Pi probe failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
