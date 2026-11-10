use super::*;
use std::fs;

#[test]
fn pressure_sampler_honours_a_two_second_configured_interval() {
    // The shared configuration parser owns the 100--60000 ms boundary.  This
    // collector must not silently reduce a valid lower-overhead cadence.
    assert_eq!(pressure_interval_ms(2_000), 2_000);
    assert_eq!(pressure_interval_ms(60_001), 60_000);
}

#[test]
fn missing_verified_scope_never_reads_host_pressure() {
    let dir =
        crate::fixture_namespace::allocate(&std::env::temp_dir(), "asb-pressure-scope-").unwrap();
    let mut session =
        Session::start_with_collectors(&dir, &["kernel-pressure".into()], None).unwrap();
    session.event(LifecycleEvent::AgentExecuteBegin).unwrap();
    session.event(LifecycleEvent::AgentExecuteEnd).unwrap();
    ADAPTER.finish(&mut session).unwrap();
    let report: serde_json::Value =
        serde_json::from_slice(&fs::read(dir.join("kernel-pressure.json")).unwrap()).unwrap();
    assert_eq!(report["status"], "unavailable");
    assert!(report["metrics"].as_object().unwrap().is_empty());
    assert!(
        report["reason"]
            .as_str()
            .unwrap()
            .contains("host pressure is excluded")
    );
    assert_eq!(
        ADAPTER.target_scope("local-docker"),
        TargetScope::AgentProcess
    );
    drop(session);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn finalization_cannot_turn_missing_execution_end_into_valid_data() {
    let dir = crate::fixture_namespace::allocate(&std::env::temp_dir(), "asb-pressure-boundary-")
        .unwrap();
    let leaf = dir.join("fixture-leaf");
    fs::create_dir(&leaf).unwrap();
    fs::write(leaf.join("memory.current"), "10").unwrap();
    let mut session =
        Session::start_with_collectors(&dir, &["kernel-pressure".into()], None).unwrap();
    session.state::<State>().borrow_mut().sampler = Some(Sampler::start(leaf, 1000).unwrap());
    ADAPTER.finish(&mut session).unwrap();
    let report: serde_json::Value =
        serde_json::from_slice(&fs::read(dir.join("kernel-pressure.json")).unwrap()).unwrap();
    assert_eq!(report["status"], "unavailable");
    assert!(report["reason"].as_str().unwrap().contains("incomplete"));
    drop(session);
    fs::remove_dir_all(dir).unwrap();
}
