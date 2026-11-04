//! Module discovery must work in a project without installed SDKs or providers.
mod support;
use std::process::Command;
use support::IsolatedEnvironment;

#[test]
fn public_discovery_exposes_pinned_protocols_and_unscored_references() {
    let root = std::env::temp_dir().join(format!("asb-module-contracts-{}", std::process::id()));
    std::fs::create_dir_all(root.join(".asb")).unwrap();
    std::fs::write(
        root.join(".asb/project.toml"),
        "tools_dir = '.asb-tools'\nworkloads_dir = 'workloads'\nworkspace_dir = 'asb-workspace'\n",
    )
    .unwrap();
    for (name, support, family) in [
        ("swe-bench-lite", "official-adapter", "swe-bench"),
        ("terminal-bench-2.1", "official-adapter", "terminal-bench"),
        ("swe-bench-pro", "reference-smoke", "swe-bench-pro"),
        ("repository-navigation", "local-task", "asb-local"),
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_asb"))
            .isolated_env()
            .current_dir(&root)
            .args(["workload", "describe", name])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["id"], name);
        assert_eq!(value["schema_version"], 1);
        assert_eq!(value["support"], support);
        assert_eq!(value["family"], family);
        if support == "reference-smoke" {
            assert_eq!(value["protocol"]["score_policy"], "unscored");
            assert!(value["dataset"].is_null());
        }
    }
    let output = Command::new(env!("CARGO_BIN_EXE_asb"))
        .isolated_env()
        .current_dir(&root)
        .args(["workload", "describe", "unknown-module"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    std::fs::remove_dir_all(root).unwrap();
}
