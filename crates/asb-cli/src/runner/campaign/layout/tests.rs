use super::*;
use crate::runner::campaign::ReplayInput;
use serde_json::{Value, json};

#[test]
fn source_replay_identity_is_optional_historical_data_not_observed_material() {
    let mut value = selected(plan(ExecutionMode::Replay, 2, 2, false), true);
    let expected = crate::cassette::Compatibility {
        agent: "source-agent".into(),
        provider: "source-provider".into(),
        model: "source-model".into(),
        workload: "source-workload".into(),
        task: "source-task".into(),
        dataset_sha256: Some(crate::cassette::digest(b"source dataset")),
        prompt_sha256: crate::cassette::digest(b"source prompt"),
        material_sha256: crate::cassette::digest(b"source material"),
        protocol_sha256: Some(crate::cassette::digest(b"source protocol")),
    };
    value.replay_compatibility = Some(expected.clone());
    // A valid source can deliberately differ from the requested configuration:
    // this is inspectable intent, and genuine material admission must refuse it.
    value.validate_layout().unwrap();
    let wire = serde_json::to_value(&value).unwrap();
    assert_eq!(
        wire["replay_compatibility"],
        serde_json::to_value(&expected).unwrap()
    );
    for field in [
        "agent",
        "provider",
        "model",
        "workload",
        "task",
        "dataset_sha256",
        "prompt_sha256",
        "material_sha256",
        "protocol_sha256",
    ] {
        let mut invalid = wire.clone();
        invalid["replay_compatibility"][field] = json!("");
        assert!(
            serde_json::from_value::<Plan>(invalid)
                .unwrap()
                .validate_layout()
                .is_err(),
            "{field}"
        );
    }
    let mut historical = wire;
    historical
        .as_object_mut()
        .unwrap()
        .remove("replay_compatibility");
    let historical: Plan = serde_json::from_value(historical).unwrap();
    historical.validate_layout().unwrap();
    assert!(historical.replay_compatibility.is_none());
    let mut legacy = plan(ExecutionMode::Replay, 1, 1, false);
    legacy.replay_compatibility = Some(expected.clone());
    assert!(legacy.validate_layout().is_err());
    for mode in [ExecutionMode::Live, ExecutionMode::Record] {
        let mut other = plan(mode, 1, 1, false);
        other.replay_compatibility = Some(expected.clone());
        assert!(other.validate_layout().is_err());
    }
}

#[test]
fn recording_capacity_is_recomputed_and_historical_absence_stays_unknown() {
    let mut value = plan(ExecutionMode::Record, 2, 3, false);
    assert!(value.validate_layout().is_ok());
    value.recording_selection = Some(crate::workloads::TaskSelection {
        module: None,
        workload: "bug-fix".into(),
        task_id: "bug-fix".into(),
        dataset: None,
        evaluation_kind: "fixture".into(),
        population_size: None,
    });
    value.recording_reservation = value.recording_capacity().unwrap();
    value.validate_layout().unwrap();
    let original = serde_json::to_value(&value).unwrap();
    for field in [
        "schema_version",
        "references",
        "reference_bytes",
        "journal_bytes",
        "manifest_bytes",
        "publication_bytes",
        "directory_bytes",
    ] {
        let mut changed = original.clone();
        let before = changed["recording_reservation"][field].as_u64().unwrap();
        changed["recording_reservation"][field] = json!(before + 1);
        assert!(
            serde_json::from_value::<Plan>(changed)
                .unwrap()
                .validate_layout()
                .is_err(),
            "{field}"
        );
    }
    for field in ["workload", "task_id", "evaluation_kind"] {
        let mut changed = original.clone();
        changed["recording_selection"][field] = json!("");
        assert!(
            serde_json::from_value::<Plan>(changed)
                .unwrap()
                .validate_layout()
                .is_err(),
            "{field}"
        );
    }
    for field in ["recording_selection", "recording_reservation"] {
        let mut changed = original.clone();
        changed.as_object_mut().unwrap().remove(field);
        assert!(
            serde_json::from_value::<Plan>(changed)
                .unwrap()
                .validate_layout()
                .is_err(),
            "{field}"
        );
    }
    let mut historical = original;
    for field in ["recording_selection", "recording_reservation"] {
        historical.as_object_mut().unwrap().remove(field);
    }
    let historical: Plan = serde_json::from_value(historical).unwrap();
    historical.validate_layout().unwrap();
    assert!(historical.recording_capacity().unwrap().is_none());
}

fn plan(mode: ExecutionMode, concurrency: usize, repeat: u64, measured: bool) -> Plan {
    let configuration: BenchmarkConfig = toml::from_str(&format!(
        "agent='pi'\nprovider='openrouter'\nmodel='fixture'\nworkload='bug-fix'\nconcurrency={concurrency}"
    )).unwrap();
    let mut plan = Plan {
        replay_compatibility: None,
        recording_selection: None,
        recording_reservation: None,
        replay_source: None,
        measurement_storage: None,
        replay_selection: None,
        recording: (mode == ExecutionMode::Record)
            .then(|| PathBuf::from("/project/recording.asb.zip")),
        admission: crate::planning::BatchAdmissionPolicy::current(),
        schema_version: 1,
        scope: "intended-executions-not-attempt-evidence".into(),
        campaign: "fixture".into(),
        source: "/project/benchmark.toml".into(),
        configuration_sha256: crate::cassette::digest(&serde_json::to_vec(&configuration).unwrap()),
        repeat,
        concurrency,
        mode,
        respect_record_timings: false,
        measurement_root: measured.then(|| PathBuf::from("/external/fixture")),
        slots: planned_slots(&configuration, repeat, measured).unwrap(),
        configuration,
        replay_inputs: Vec::new(),
    };
    if mode == ExecutionMode::Replay {
        for replica in 0..concurrency {
            plan.replay_inputs.push(ReplayInput {
                recorder: None,
                complete: None,
                track: None,
                source: format!("/project/legacy-{replica}.jsonl").into(),
                private: format!("inputs/recording-{}.jsonl", replica + 1).into(),
                local_tools: None,
                tool_protocol: None,
                deterministic_manifest: None,
                native_trace: None,
                native_tool: None,
                mcp_relays: Vec::new(),
                bytes: 42,
                sha256: "a".repeat(64),
            });
        }
        for slot in &mut plan.slots {
            slot.replay_input = Some(slot.replica - 1);
        }
    }
    plan
}

#[test]
fn measurement_storage_is_explicit_bounded_and_legacy_absence_remains_unknown() {
    use crate::runner::campaign::MeasurementStorage;
    let mut value = plan(ExecutionMode::Live, 1, 2, true);
    for storage in [MeasurementStorage::Campaign, MeasurementStorage::Override] {
        value.measurement_storage = Some(storage);
        assert!(value.validate_layout().is_ok());
        value.measurement_root = Some("/external/foreign-campaign".into());
        assert!(value.validate_layout().is_err());
        value.measurement_root = Some("/external/fixture".into());
    }
    value.measurement_storage = None;
    value.measurement_root = Some("/external/legacy-location".into());
    assert!(value.validate_layout().is_ok());
    value.measurement_storage = Some(MeasurementStorage::Campaign);
    value.measurement_root = None;
    assert!(value.validate_layout().is_err());
}

fn selected(mut plan: Plan, bundled: bool) -> Plan {
    plan.replay_inputs.truncate(1);
    let input = &mut plan.replay_inputs[0];
    input.track = Some(64);
    input.private = "inputs/track-64.jsonl".into();
    // Relative legacy sources are metadata, not authority for guessed I/O.
    input.source = if bundled {
        "/project/bundle.zip"
    } else {
        "relative-legacy-concurrency-64.jsonl"
    }
    .into();
    input.complete = bundled.then_some(true);
    for slot in &mut plan.slots {
        slot.replay_input = Some(0);
        slot.replay_track = Some(64);
    }
    if bundled {
        plan.replay_selection = Some(crate::cassette::SelectionPlan {
            algorithm: "sha256-counter-rejection-v1".into(),
            seed: 19,
            manifest_sha256: "b".repeat(64),
            selector: None,
            tracks: vec![64; plan.slots.len()],
        });
    }
    plan
}

fn rejects(wire: Value) {
    let decoded = serde_json::from_value::<Plan>(wire);
    assert!(decoded.is_err() || decoded.unwrap().validate_layout().is_err());
}

#[test]
fn emitted_layouts_cover_all_modes_counts_and_external_measurement_roots() {
    for mode in [
        ExecutionMode::Live,
        ExecutionMode::Record,
        ExecutionMode::Replay,
    ] {
        for (concurrency, repeat) in [(1, 1), (2, 3), (64, 2)] {
            for measured in [false, true] {
                let plan = plan(mode, concurrency, repeat, measured);
                assert_eq!(
                    plan.validate_layout().unwrap(),
                    concurrency * repeat as usize
                );
                let bytes = serde_json::to_vec(&plan).unwrap();
                let decoded: Plan = serde_json::from_slice(&bytes).unwrap();
                assert_eq!(decoded.validate_layout().unwrap(), plan.slots.len());
                if mode == ExecutionMode::Replay {
                    for bundled in [false, true] {
                        let decoded: Plan = serde_json::from_slice(&bytes).unwrap();
                        assert!(selected(decoded, bundled).validate_layout().is_ok());
                    }
                }
            }
        }
    }
    for (repeat, concurrency) in [(0, 1), (1, 0), (1, 65), (u64::MAX, 64), (65_534, 1)] {
        assert!(population(repeat, concurrency).is_err());
    }
}

#[test]
fn shared_slot_paths_match_existing_runner_replica_configuration() {
    for concurrency in 1..=crate::benchmark::MAX_CONCURRENCY {
        let plan = plan(ExecutionMode::Live, concurrency, 2, true);
        for actual in &plan.slots {
            let mut configured = plan.configuration.clone();
            configured.workspace = format!("batch-{}/workspace", actual.batch);
            configured.log_file = format!("batch-{}/run.jsonl", actual.batch);
            configured.measurements = Some(crate::benchmark::MeasurementConfig {
                enabled: true,
                directory: format!("batch-{}/measurements", actual.batch),
                collectors: Vec::new(),
            });
            let replica = crate::runner::replica::configuration(
                &configured,
                crate::runner::replica::Identity {
                    index: actual.replica,
                    total: concurrency,
                },
            )
            .unwrap();
            assert_eq!(
                actual.workspace.as_os_str(),
                std::ffi::OsStr::new(&replica.workspace)
            );
            assert_eq!(
                actual.log.as_os_str(),
                std::ffi::OsStr::new(&replica.log_file)
            );
            assert_eq!(
                actual.measurements.as_ref().unwrap().as_os_str(),
                std::ffi::OsStr::new(&replica.measurements.unwrap().directory)
            );
        }
    }
}

#[test]
fn structural_mutations_reject_without_observing_any_artifacts() {
    let original = serde_json::to_value(plan(ExecutionMode::Replay, 2, 3, true)).unwrap();
    for (field, bad) in [
        ("schema_version", json!(2)),
        ("scope", json!("other")),
        ("campaign", json!("../fixture")),
        ("campaign", json!("")),
        ("mode", json!("unknown")),
        ("mode", json!("live")),
        ("repeat", json!(0)),
        ("repeat", json!(u64::MAX)),
        ("concurrency", json!(1)),
        ("concurrency", json!(65)),
        ("configuration_sha256", json!("f".repeat(64))),
        ("source", json!("")),
        ("recording", json!("/foreign.zip")),
        ("measurement_root", json!("relative")),
        ("measurement_root", Value::Null),
        ("slots", json!([])),
        ("replay_inputs", json!([])),
    ] {
        let mut wire = original.clone();
        wire[field] = bad;
        rejects(wire);
    }
    for slot in 0..6 {
        for (field, bad) in [
            ("id", json!(0)),
            ("batch", json!(99)),
            ("replica", json!(0)),
            ("replay_input", json!(99)),
            ("replay_input", Value::Null),
            ("replay_track", json!(7)),
            ("measurements", Value::Null),
        ] {
            let mut wire = original.clone();
            wire["slots"][slot][field] = bad;
            rejects(wire);
        }
        for field in ["workspace", "log", "measurements"] {
            for bad in [
                "../outside",
                "/absolute",
                "batch-1/../batch-1/workspace",
                "./batch-1/workspace-concurrency-1",
                "",
                "batch-1/workspace-concurrency-1/child",
            ] {
                let mut wire = original.clone();
                wire["slots"][slot][field] = json!(bad);
                rejects(wire);
            }
        }
    }
    let mut duplicate = original.clone();
    duplicate["slots"][1] = duplicate["slots"][0].clone();
    rejects(duplicate);
    let mut reordered = original.clone();
    reordered["slots"].as_array_mut().unwrap().swap(0, 1);
    rejects(reordered);
    let mut missing = original.clone();
    missing["slots"].as_array_mut().unwrap().pop();
    rejects(missing);
    for (field, bad) in [
        ("private", json!("../input.jsonl")),
        ("private", json!("/input.jsonl")),
        ("private", json!("./inputs/recording-1.jsonl")),
        ("private", json!("inputs/recording-2.jsonl")),
        ("sha256", json!("F".repeat(64))),
        ("sha256", json!("bad")),
        ("bytes", json!(u64::MAX)),
        ("track", json!(7)),
        ("complete", json!(true)),
        ("source", json!("")),
    ] {
        let mut wire = original.clone();
        wire["replay_inputs"][0][field] = bad;
        rejects(wire);
    }
    let mut unused = original.clone();
    let mut input = unused["replay_inputs"][0].clone();
    input["private"] = json!("inputs/recording-3.jsonl");
    unused["replay_inputs"].as_array_mut().unwrap().push(input);
    rejects(unused);
    let mut changed_replica = original.clone();
    changed_replica["slots"][2]["replay_input"] = json!(1);
    rejects(changed_replica);
}

#[test]
fn bundle_choices_and_numeric_incomplete_status_remain_distinct() {
    let mut bundle = selected(plan(ExecutionMode::Replay, 2, 3, false), true);
    bundle.replay_inputs[0].complete = Some(false);
    assert!(bundle.validate_layout().is_err());
    bundle.replay_selection.as_mut().unwrap().selector = Some(crate::cassette::Selector::Id(64));
    assert!(bundle.validate_layout().is_ok());
    let wire = serde_json::to_value(bundle).unwrap();
    for (field, bad) in [
        ("algorithm", json!("unknown")),
        ("manifest_sha256", json!("bad")),
        ("tracks", json!([64])),
        ("tracks", json!([64, 64, 65, 64, 64, 64])),
        ("selector", json!({"id":7})),
    ] {
        let mut changed = wire.clone();
        changed["replay_selection"][field] = bad;
        rejects(changed);
    }
    let mut changed = wire.clone();
    changed["replay_inputs"][0]["complete"] = Value::Null;
    rejects(changed);
    let mut changed = wire.clone();
    changed["replay_inputs"][0]["track"] = json!(0);
    rejects(changed);
    let mut changed = wire.clone();
    changed["slots"][0]["replay_track"] = json!(7);
    rejects(changed);
    let mut legacy = selected(plan(ExecutionMode::Replay, 2, 3, false), false);
    assert!(legacy.validate_layout().is_ok());
    legacy.replay_inputs[0].complete = Some(true);
    assert!(legacy.validate_layout().is_err());
}

#[test]
fn selected_legacy_keeps_backend_limits_without_reading_or_allocating_payloads() {
    let mut numeric = selected(plan(ExecutionMode::Replay, 1, 1, false), false);
    numeric.replay_inputs[0].bytes = 65 * 1024 * 1024;
    assert!(numeric.validate_layout().is_ok());
    let max = crate::cassette::Limits::default().max_track_bytes;
    numeric.replay_inputs[0].bytes = max;
    assert!(numeric.validate_layout().is_ok());
    numeric.replay_inputs[0].bytes = max + 1;
    assert!(numeric.validate_layout().is_err());
    let mut legacy = plan(ExecutionMode::Replay, 1, 1, false);
    legacy.replay_inputs[0].bytes = 65 * 1024 * 1024;
    assert!(legacy.validate_layout().is_err());
    let mut bundle = plan(ExecutionMode::Replay, 9, 1, false);
    for (index, input) in bundle.replay_inputs.iter_mut().enumerate() {
        input.track = Some(index as u64 + 1);
        input.complete = Some(true);
        input.private = format!("inputs/track-{}.jsonl", index + 1).into();
        input.source = "/project/bundle.zip".into();
        input.bytes = max;
    }
    for slot in &mut bundle.slots {
        slot.replay_track = Some(slot.replica as u64);
    }
    bundle.replay_selection = Some(crate::cassette::SelectionPlan {
        algorithm: "sha256-counter-rejection-v1".into(),
        seed: 19,
        manifest_sha256: "b".repeat(64),
        selector: None,
        tracks: (1..=9).collect(),
    });
    assert!(
        bundle
            .validate_layout()
            .unwrap_err()
            .to_string()
            .contains("aggregate")
    );
}
