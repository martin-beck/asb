use super::observations::{Kind, Scope};
use super::*;
use crate::measurements::ComponentTiming;

#[test]
fn missing_measurements_are_assessed_without_fabricating_zero_or_causes() {
    let report = analyze(None);
    assert!(report.observations.iter().all(|o| o.value.is_none()));
    assert!(
        report
            .findings
            .iter()
            .all(|f| matches!(f.assessment, Assessment::InsufficientEvidence))
    );
    assert!(report.findings.iter().all(|f| f.evidence.is_empty()));
    let empty = analyze(Some(&ComponentTiming::default()));
    assert_eq!(empty.observations[0].value, Some(0.0));
    assert!(empty.observations[1..].iter().all(|o| o.value.is_none()));
}

#[test]
fn concurrent_thread_time_can_exceed_wall_and_keeps_its_own_semantics() {
    let report = analyze(Some(&ComponentTiming {
        wall_ms: 100,
        cpu_user_ms: Some(800.0),
        cpu_system_ms: Some(200.0),
        scheduler_wait_ms: Some(300.0),
        io_wait_ms: Some(0.0),
        cpu_accounting_source: Some("cgroup-v2".into()),
        ..Default::default()
    }));
    assert_eq!(report.observations[0].kind, Kind::ElapsedWallTime);
    assert_eq!(report.observations[1].kind, Kind::AccumulatedThreadTime);
    assert_eq!(report.observations[1].scope, Scope::AgentProcessTree);
    assert_eq!(report.observations[3].value, Some(300.0));
    assert!(matches!(
        report.findings[0].assessment,
        Assessment::Observed
    ));
    assert!(matches!(
        report.findings[1].assessment,
        Assessment::NotObserved
    ));
    assert!(matches!(
        report.findings[2].assessment,
        Assessment::Observed
    ));
    assert_eq!(report.findings[0].evidence, vec!["scheduler.wait_ms"]);
}

#[test]
fn invalid_observations_remain_unavailable_and_foreign_cpu_scopes_are_unknown() {
    let report = analyze(Some(&ComponentTiming {
        cpu_user_ms: Some(f64::INFINITY),
        cpu_system_ms: Some(f64::NAN),
        scheduler_wait_ms: Some(-1.0),
        cpu_accounting_source: Some("unrecognized-accounting".into()),
        ..Default::default()
    }));
    assert!(report.observations[1..].iter().all(|o| o.value.is_none()));
    assert_eq!(report.observations[1].scope, Scope::Unknown);
    assert!(serde_json::to_string(&report).is_ok());
}

#[test]
fn artifact_publication_is_deterministic_and_contains_evidence_references() {
    let root =
        crate::fixture_namespace::allocate(&std::env::temp_dir(), "asb-bottleneck-").unwrap();
    publish(&root, None).unwrap();
    let first = std::fs::read(root.join("bottlenecks.json")).unwrap();
    publish(&root, None).unwrap();
    assert_eq!(first, std::fs::read(root.join("bottlenecks.json")).unwrap());
    let report: serde_json::Value = serde_json::from_slice(&first).unwrap();
    assert_eq!(report["schema_version"], 1);
    assert_eq!(
        report["observations"][0]["source_artifact"],
        "components.json"
    );
    std::fs::remove_dir_all(root).unwrap();
}
