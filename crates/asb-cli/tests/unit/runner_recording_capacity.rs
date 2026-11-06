//! Material-gate capacity admission only, not SDK or client qualification.
use super::*;

fn planned(root: &Path, destination: PathBuf) -> crate::runner::campaign::Plan {
    let configuration: crate::benchmark::BenchmarkConfig =
        toml::from_str("agent='fixture'\nprovider='fixture'\nmodel='fixture'\nworkload='fixture'")
            .unwrap();
    let mut plan = crate::runner::campaign::Plan {
        ignore_evaluation: false,
        ignore_agent: false,
        ignore_collector: false,
        ignore_provider_mismatch: false,
        normalization_policy: None,
        replay_compatibility: None,
        recording_selection: Some(workloads::TaskSelection {
            module: None,
            workload: "fixture".into(),
            task_id: "fixture".into(),
            dataset: None,
            evaluation_kind: "fixture".into(),
            population_size: None,
        }),
        recording_reservation: None,
        replay_source: None,
        measurement_storage: None,
        replay_selection: None,
        recording: Some(destination),
        admission: crate::planning::BatchAdmissionPolicy::current(),
        schema_version: 1,
        scope: "intended-executions-not-attempt-evidence".into(),
        campaign: "campaign-1".into(),
        source: root.join("benchmark.toml"),
        configuration_sha256: crate::cassette::digest(&serde_json::to_vec(&configuration).unwrap()),
        repeat: 1,
        concurrency: 1,
        mode: crate::planning::ExecutionMode::Record,
        max_steps: None,
        respect_record_timings: false,
        measurement_root: None,
        replay_inputs: Vec::new(),
        slots: crate::runner::campaign::planned_slots(&configuration, 1, false).unwrap(),
        configuration,
    };
    plan.recording_reservation = plan.recording_capacity().unwrap();
    plan.validate_layout().unwrap();
    plan
}

#[test]
fn genuine_identity_is_checked_before_creating_a_journal_and_shape_hashes_never_persist() {
    let fixture = Fixture::new();
    let root = fixture.0.canonicalize().unwrap();
    let identity = recording_identity(&result());
    for field in 0..5 {
        let destination = root.join(format!("refused-{field}.zip"));
        let plan = planned(&root, destination.clone());
        let recorder = crate::runner::recording::Recorder::new(
            destination.clone(),
            root.join("campaign-1"),
            "campaign-1".into(),
            1,
            1,
            Some(&plan),
        );
        let mut changed = identity.clone();
        let value = match field {
            0 => &mut changed.agent,
            1 => &mut changed.provider,
            2 => &mut changed.model,
            3 => &mut changed.workload,
            _ => &mut changed.task,
        };
        // Equal lengths must not turn name identity into a byte-budget check.
        *value = "z".repeat(value.len());
        assert!(recorder.prepare(&changed, 1, &[1]).is_err());
        assert!(!destination.exists());
        assert!(!root.join(format!(".refused-{field}.zip.partial")).exists());
        assert!(
            !root
                .join(format!(".refused-{field}.zip.recording.lock"))
                .exists()
        );
    }
    let destination = root.join("observed.zip");
    let plan = planned(&root, destination.clone());
    let recorder = crate::runner::recording::Recorder::new(
        destination,
        root.join("campaign-1"),
        "campaign-1".into(),
        1,
        1,
        Some(&plan),
    );
    let mut observed = identity;
    observed.material_sha256 = "f".repeat(64);
    observed.prompt_sha256 = "e".repeat(64);
    recorder.prepare(&observed, 1, &[1]).unwrap();
    let intent: serde_json::Value = serde_json::from_slice(
        &std::fs::read(root.join(".observed.zip.partial/intent.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        intent["manifest"]["compatibility"]["material_sha256"],
        observed.material_sha256
    );
    assert_eq!(
        intent["manifest"]["compatibility"]["prompt_sha256"],
        observed.prompt_sha256
    );
}
