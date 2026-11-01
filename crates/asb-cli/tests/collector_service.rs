//! Collector private-service routing preserves adapter-owned bootstrap checks.
use std::process::{Command, Stdio};

#[test]
fn private_collector_service_dispatches_only_its_owner() {
    for args in [
        vec!["__asb-collector-service"],
        vec!["__asb-collector-service", "unknown"],
        vec!["__asb-collector-service", "runtime"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_asb"))
            .args(args)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2));
    }
    let invoke = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_asb"))
            .args(args)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    };
    let current = invoke(&["__asb-collector-service", "perfetto"]);
    let legacy = invoke(&["__asb-perfetto-probes"]);
    assert_eq!(current.status.code(), Some(2));
    assert_eq!(current.status.code(), legacy.status.code());
    assert_eq!(current.stderr, legacy.stderr);
    assert!(
        String::from_utf8_lossy(&current.stderr).contains("Perfetto isolated bootstrap failed")
    );
}
