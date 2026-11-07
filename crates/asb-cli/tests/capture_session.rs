//! Provider-free exact-byte recording and finalization fault qualification.
mod support;
use support::IsolatedEnvironment;

#[test]
fn recorder_accounting_and_publication_fault_matrix() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let output = std::process::Command::new("node")
        .arg("--test")
        .arg(root.join("tests/capture_session.mjs"))
        .arg(root.join("tests/capture_http.mjs"))
        .arg(root.join("tests/tape_normalization.mjs"))
        .arg(root.join("tests/tool_result_normalization.mjs"))
        .arg(root.join("tests/normalizer_supervisor.mjs"))
        .arg(root.join("tests/sympy_normalizer.mjs"))
        .arg(root.join("tests/workload_normalizer_examples.mjs"))
        .arg(root.join("tests/replay_normalizer_discovery.mjs"))
        .arg(root.join("tests/replay_discovery_store.mjs"))
        .arg(root.join("tests/agent_profile_normalizers.mjs"))
        .arg(root.join("tests/tape_stream_paths.mjs"))
        .current_dir(root)
        .isolated_env()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .output()
        .expect("Node must be installed for the embedded recorder tests");
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
