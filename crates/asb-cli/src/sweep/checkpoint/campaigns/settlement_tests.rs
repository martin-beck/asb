//! Synthetic metadata qualification; actual CLI attempts live in repeated_sweep.
use super::*;
use crate::sweep::checkpoint::{Inputs, Snapshotter};
use serde_json::{Value, json};

fn fixture() -> (Fixture, SweepPlan, PathBuf, Inputs) {
    fixture_mode(crate::tape::Mode::Replay)
}

fn fixture_mode(mode: crate::tape::Mode) -> (Fixture, SweepPlan, PathBuf, Inputs) {
    fixture_options(mode, false, false)
}

fn fixture_options(
    mode: crate::tape::Mode,
    remote: bool,
    alias: bool,
) -> (Fixture, SweepPlan, PathBuf, Inputs) {
    fixture_concurrency(mode, remote, alias, 1)
}

pub(in crate::sweep::checkpoint) fn fixture_concurrency(
    mode: crate::tape::Mode,
    remote: bool,
    alias: bool,
    concurrency: usize,
) -> (Fixture, SweepPlan, PathBuf, Inputs) {
    use crate::workloads::contracts::{EnvironmentKind, HarnessIdentity, SupportLevel};
    let (fixture, mut plan, results) = published_concurrency(mode, concurrency);
    if remote {
        for entry in &mut plan.entries {
            let dataset = crate::workloads::session::DatasetIdentity {
                source: "fixture/dataset".into(),
                revision: "pinned".into(),
                split: "test".into(),
            };
            entry.selection.dataset = Some(dataset.clone());
            entry.selection.population_size = Some(4);
            entry.selection.evaluation_kind = "upstream-harness".into();
            let module = entry.selection.module.as_mut().unwrap();
            module.support = SupportLevel::OfficialAdapter;
            module.dataset = Some(dataset);
            module.harness = Some(HarnessIdentity {
                id: "fixture-reader".into(),
                package: "fixture-reader==1".into(),
            });
            module.protocol.environment = EnvironmentKind::LocalDocker;
            assert!(entry.selection.valid());
        }
    }
    if alias {
        use std::os::unix::fs::symlink;
        let foreign = fixture.0.join("foreign-project");
        fs::create_dir_all(foreign.join(".asb")).unwrap();
        fs::copy(
            fixture.0.join(".asb/project.toml"),
            foreign.join(".asb/project.toml"),
        )
        .unwrap();
        for entry in &mut plan.entries {
            let source = fixture.0.join(&entry.reference);
            let target = foreign.join(&entry.reference);
            fs::rename(&source, &target).unwrap();
            symlink(&target, &source).unwrap();
            let reference = entry.campaign.as_mut().unwrap();
            let path = results.join(&reference.root).join("plan.json");
            let mut value: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            value["source"] = json!(target);
            let bytes = serde_json::to_vec_pretty(&value).unwrap();
            reference.bytes = bytes.len() as u64;
            reference.sha256 = crate::cassette::digest(&bytes);
            fs::write(path, bytes).unwrap();
        }
    }
    fs::write(
        results.join("plan.json"),
        serde_json::to_vec_pretty(&plan).unwrap(),
    )
    .unwrap();
    for (configuration, layout) in plan.read_campaigns(&results).unwrap().iter().enumerate() {
        let entry = &plan.entries[configuration];
        for slot in &layout.plan.slots {
            let batch = layout.root.join(format!("batch-{}", slot.batch));
            fs::create_dir_all(batch.join("starts")).unwrap();
            fs::create_dir_all(batch.join("outcomes")).unwrap();
            fs::create_dir_all(batch.join(".asb-completions")).unwrap();
            let workspace = layout.root.join(&slot.workspace);
            fs::create_dir_all(&workspace).unwrap();
            fs::write(layout.root.join(&slot.log), "fixture log").unwrap();
            let mut task = crate::workloads::TaskIdentity::local("bug-fix", None);
            task.dataset = entry.selection.dataset.clone();
            let mut workload_evidence = Vec::new();
            let compatibility = crate::cassette::Compatibility {
                agent: "pi".into(),
                provider: "local".into(),
                model: "fixture".into(),
                workload: "bug-fix".into(),
                task: "bug-fix".into(),
                dataset_sha256: crate::runner::dataset_fingerprint(task.dataset.as_ref()).unwrap(),
                prompt_sha256: crate::cassette::digest(b"fixture prompt"),
                material_sha256: crate::cassette::digest(b"fixture material"),
                protocol_sha256: Some(
                    crate::runner::replay_protocol_fingerprint(
                        entry.selection.module.as_ref(),
                        &Default::default(),
                    )
                    .unwrap(),
                ),
            };
            let cohort = json!({"id":format!("abc-{configuration:x}-{:x}", slot.batch),"selected_replicas":concurrency,"admitted_slots":(1..=concurrency).collect::<Vec<_>>()});
            let execution = layout.plan.execution(slot);
            if remote {
                let preparation =
                    crate::workloads::TaskIdentity::local("preparation", None).attempt_id;
                assert_ne!(preparation, task.attempt_id);
                let artifacts = fixture.0.join(".asb/attempts").join(&preparation);
                fs::create_dir_all(&artifacts).unwrap();
                fs::write(
                    artifacts.join("closed.json"),
                    serde_json::to_vec(&json!({"attempt_id":task.attempt_id})).unwrap(),
                )
                .unwrap();
                let reference = crate::workloads::recovery::AttemptReference {
                    schema_version: 1,
                    preparation_id: preparation.clone(),
                    workload: "bug-fix".into(),
                    task_id: "bug-fix".into(),
                    dataset: task.dataset.clone().unwrap(),
                    artifacts: artifacts.clone(),
                };
                fs::create_dir_all(workspace.join(".asb-attempts")).unwrap();
                fs::write(
                    workspace
                        .join(".asb-attempts")
                        .join(format!("{preparation}.json")),
                    serde_json::to_vec(&reference).unwrap(),
                )
                .unwrap();
                fs::create_dir_all(batch.join("allocations")).unwrap();
                fs::write(batch.join(format!("allocations/{}.json", slot.replica)), serde_json::to_vec(&json!({"schema_version":1,"scope":"allocated-workload-reference-not-outcome","execution":execution,"reference":reference})).unwrap()).unwrap();
                workload_evidence.push(artifacts);
            }
            fs::write(batch.join(format!("starts/{}.json", slot.replica)), serde_json::to_vec(&json!({"schema_version":1,"scope":"execution-start-intent-not-actual-attempt","execution":execution,"execution_cohort":cohort})).unwrap()).unwrap();
            fs::write(batch.join("prepared.json"), serde_json::to_vec(&json!({"schema_version":1,"scope":"verified-prepared-material","compatibility":compatibility})).unwrap()).unwrap();
            let value = json!({"schema_version":2,"scope":"task-outcome-not-yet-a-resume-receipt",
                "replica":{"index":slot.replica,"total":concurrency},"outputs":[workspace,layout.root.join(&slot.log)],
                "workload_cleanup_confirmed":true,"proxy_cleanup_confirmed":true,"workload_evidence":workload_evidence,
                "summary":{"schema_version":2,"agent":"pi","provider":"local","model":"fixture","workload":"bug-fix",
                    "task":task,"execution":execution,"execution_cohort":cohort,"replica_index":slot.replica,
                    "module":entry.selection.module,"process_success":false,"workload_success":false,
                    "workload_evaluation":entry.selection.evaluation_kind,"workload_baseline":{"kind":"measured","passed":false}}});
            let bytes = serde_json::to_vec(&value).unwrap();
            let relative = PathBuf::from(format!(
                "batch-{}/.asb-completions/{}.json",
                slot.batch,
                if concurrency == 1 {
                    "fixture".into()
                } else {
                    format!("fixture-{}", slot.replica)
                }
            ));
            fs::write(layout.root.join(&relative), &bytes).unwrap();
            fs::write(
                batch.join(format!("outcomes/{}.json", slot.replica)),
                serde_json::to_vec(&json!({
                    "schema_version":1,"execution":execution,"path":relative,"bytes":bytes.len(),
                    "sha256":crate::cassette::digest(&bytes),"task":task
                }))
                .unwrap(),
            )
            .unwrap();
        }
    }
    let state = inputs_environment(&plan, &results);
    (fixture, plan, results, state)
}

fn inputs(plan: &SweepPlan, results: &Path) -> Inputs {
    inputs_environment(plan, results)
}

fn inputs_environment(plan: &SweepPlan, results: &Path) -> Inputs {
    let directory = results.join(".checkpoint");
    let mut roots = input_roots(results, plan).unwrap();
    roots.push(crate::sweep::checkpoint::Root::path(
        "selection-plan",
        results.join("plan.json"),
    ));
    let context = json!({"scope":"settlement-test-not-runtime-closure"});
    let snapshotter = Snapshotter::open(&directory, "python3".into()).unwrap();
    let initial = snapshotter
        .fingerprint(&roots, &context, false, &[])
        .unwrap();
    let state = Inputs {
        directory,
        roots,
        context,
        snapshotter,
        initial,
        runtime: None,
    };
    state.publish("pending", None).unwrap();
    state.reserve_campaigns(results, plan).unwrap();
    state
}

fn untouched(mode: crate::tape::Mode) -> (Fixture, SweepPlan, PathBuf, Inputs) {
    let (fixture, plan, results) = published(mode);
    let state = inputs(&plan, &results);
    (fixture, plan, results, state)
}

#[test]
fn untouched_classification_keeps_original_reservations_and_ownership_without_attempts() {
    let (fixture, plan, results, state) = untouched(crate::tape::Mode::Replay);
    for layout in plan.read_campaigns(&results).unwrap() {
        // A shared batch/preparation directory is not an agent start or a
        // fresh-material proof. Generation activation validates its own material.
        fs::create_dir_all(layout.root.join("batch-1/.asb-completions")).unwrap();
        fs::write(layout.root.join("batch-1/prepared.json"), "{}").unwrap();
    }
    let before = fs::read(results.join(".checkpoint/inputs.json")).unwrap();
    let classification = state
        .classify_campaigns(&fixture.0, &results, &plan)
        .unwrap();
    assert_eq!(
        classification.pending_keys(),
        [(0, 1), (0, 2), (1, 1), (1, 2)].into()
    );
    for layout in plan.read_campaigns(&results).unwrap() {
        assert!(
            crate::output_lock::OutputLock::acquire_existing(
                &layout.root.join(".owner.lock"),
                "busy"
            )
            .is_err()
        );
    }
    drop(classification);
    for layout in plan.read_campaigns(&results).unwrap() {
        assert!(
            crate::output_lock::OutputLock::acquire_existing(
                &layout.root.join(".owner.lock"),
                "busy"
            )
            .is_ok()
        );
        assert!(!layout.root.join("batch-1/starts").exists());
    }
    assert_eq!(
        fs::read(results.join(".checkpoint/inputs.json")).unwrap(),
        before
    );
    assert!(!results.join(".checkpoint/campaign-completed").exists());
}

#[test]
fn partial_classification_never_recreates_missing_or_busy_original_owner() {
    for missing in [false, true] {
        let (fixture, plan, results, state) = untouched(crate::tape::Mode::Replay);
        let layouts = plan.read_campaigns(&results).unwrap();
        let path = layouts[1].root.join(".owner.lock");
        let owner = if missing {
            fs::remove_file(&path).unwrap();
            None
        } else {
            Some(crate::output_lock::OutputLock::acquire_existing(&path, "fixture").unwrap())
        };
        assert!(
            state
                .classify_campaigns(&fixture.0, &results, &plan)
                .is_err()
        );
        assert_eq!(path.exists(), !missing);
        assert!(
            crate::output_lock::OutputLock::acquire_existing(
                &layouts[0].root.join(".owner.lock"),
                "released earlier lock"
            )
            .is_ok()
        );
        assert!(!results.join(".checkpoint/campaign-completed").exists());
        drop(owner);
    }
}

#[test]
fn orphan_duplicate_partial_foreign_and_aliased_actual_records_cannot_become_untouched() {
    for case in [
        "orphan",
        "duplicate",
        "tmp",
        "foreign",
        "foreign-batch",
        "symlink",
        "directory-alias",
        "malformed",
    ] {
        let (fixture, plan, results, state) = untouched(crate::tape::Mode::Replay);
        let layouts = plan.read_campaigns(&results).unwrap();
        let layout = &layouts[1];
        let directory = layout.root.join("batch-2/.asb-completions");
        fs::create_dir_all(&directory).unwrap();
        let mut record =
            json!({"summary":{"execution":layout.plan.execution(&layout.plan.slots[1])}});
        if case == "foreign" {
            record["summary"]["execution"]["campaign"] = json!("foreign");
        }
        if case == "foreign-batch" {
            record["summary"]["execution"] = json!(layout.plan.execution(&layout.plan.slots[0]));
        }
        let path = directory.join(if case == "tmp" {
            ".pending.tmp"
        } else {
            "actual.json"
        });
        fs::write(
            &path,
            if case == "malformed" {
                b"{".to_vec()
            } else {
                serde_json::to_vec(&record).unwrap()
            },
        )
        .unwrap();
        if case == "duplicate" {
            fs::copy(&path, directory.join("duplicate.json")).unwrap();
        }
        if case == "symlink" {
            let target = fixture.0.join("foreign-record.json");
            fs::rename(&path, &target).unwrap();
            std::os::unix::fs::symlink(target, &path).unwrap();
        }
        if case == "directory-alias" {
            let target = fixture.0.join("foreign-records");
            fs::rename(&directory, &target).unwrap();
            std::os::unix::fs::symlink(target, &directory).unwrap();
        }
        assert!(
            state
                .classify_campaigns(&fixture.0, &results, &plan)
                .is_err(),
            "{case}"
        );
        for layout in &layouts {
            assert!(!layout.root.join("batch-1/starts").exists(), "{case}");
        }
        assert!(
            !results.join(".checkpoint/campaign-completed").exists(),
            "{case}"
        );
        assert!(
            !results.join(".checkpoint/campaign-unsettled").exists(),
            "{case}"
        );
    }
}

#[test]
fn each_slot_owned_observation_blocks_pending_admission_in_a_later_configuration() {
    for case in [
        "start",
        "allocation",
        "workspace",
        "runtime",
        "log",
        "raw",
        "association",
        "unsettled",
        "dangling-start",
        "dangling-workspace",
    ] {
        let (fixture, plan, results, state) = untouched(if case == "association" {
            crate::tape::Mode::Record
        } else {
            crate::tape::Mode::Replay
        });
        let layouts = plan.read_campaigns(&results).unwrap();
        let layout = &layouts[1];
        let slot = &layout.plan.slots[1];
        let path = match case {
            "start" | "dangling-start" => layout.root.join("batch-2/starts/1.json"),
            "allocation" => layout.root.join("batch-2/allocations/1.json"),
            "workspace" | "dangling-workspace" => layout.root.join(&slot.workspace),
            "runtime" => crate::runner::replica::runtime_path(&layout.root.join(&slot.workspace)),
            "log" => layout.root.join(&slot.log),
            "raw" => crate::runner::recording::producer(
                &layout.root,
                slot.batch,
                slot.replica,
                layout.plan.concurrency,
            ),
            "association" => crate::runner::recording::association_for(
                layout.plan.recording.as_ref().unwrap(),
                slot.id,
            )
            .unwrap(),
            "unsettled" => results.join(".checkpoint/campaign-unsettled/000001/000002.json"),
            _ => unreachable!(),
        };
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        if case.starts_with("dangling") {
            std::os::unix::fs::symlink("absent", &path).unwrap();
        } else if matches!(case, "workspace" | "runtime") {
            fs::create_dir(&path).unwrap();
        } else {
            fs::write(&path, "retained sentinel").unwrap();
        }
        assert!(
            state
                .classify_campaigns(&fixture.0, &results, &plan)
                .is_err(),
            "{case}"
        );
        assert!(fs::symlink_metadata(path).is_ok(), "{case}");
        assert!(!layouts[0].root.join("batch-1/starts").exists(), "{case}");
        assert!(
            !results.join(".checkpoint/campaign-completed").exists(),
            "{case}"
        );
    }
}

#[test]
fn rooted_completion_listing_is_bounded_and_distinguishes_stale_errno_from_failure() {
    let (_fixture, plan, results, _state) = untouched(crate::tape::Mode::Replay);
    let layouts = plan.read_campaigns(&results).unwrap();
    let root = &layouts[0].root;
    fs::create_dir(root.join("empty")).unwrap();
    errno::set_errno(errno::Errno(libc::ENOENT));
    assert!(
        crate::runner::campaign::reading::list_relative(root, Path::new("empty"), 0)
            .unwrap()
            .is_empty()
    );
    fs::write(root.join("empty/one.json"), "{}").unwrap();
    assert!(crate::runner::campaign::reading::list_relative(root, Path::new("empty"), 0).is_err());
    assert_eq!(
        crate::runner::campaign::reading::list_relative(root, Path::new("empty"), 1)
            .unwrap()
            .len(),
        1
    );
    assert!(
        crate::runner::campaign::reading::list_relative(root, Path::new("empty"), usize::MAX)
            .is_err()
    );
    assert!(
        crate::runner::campaign::reading::list_relative(root, Path::new("../empty"), 1).is_err()
    );
}

#[test]
fn remote_allocation_uses_original_invocation_and_actual_closed_attempt_linkage() {
    for case in ["valid", "alias", "missing", "remembered", "closed"] {
        let (fixture, plan, results, state) =
            fixture_options(crate::tape::Mode::Replay, true, case == "alias");
        let layouts = plan.read_campaigns(&results).unwrap();
        let allocation_path = layouts[1].root.join("batch-2/allocations/1.json");
        let allocation: Value =
            serde_json::from_slice(&fs::read(&allocation_path).unwrap()).unwrap();
        let artifacts = PathBuf::from(allocation["reference"]["artifacts"].as_str().unwrap());
        match case {
            "missing" => fs::remove_file(&allocation_path).unwrap(),
            "remembered" => {
                let path = layouts[1]
                    .root
                    .join("batch-2/workspace/.asb-attempts")
                    .join(format!(
                        "{}.json",
                        allocation["reference"]["preparation_id"].as_str().unwrap()
                    ));
                let mut value = allocation["reference"].clone();
                value["preparation_id"] = json!("ffff");
                fs::write(path, serde_json::to_vec(&value).unwrap()).unwrap();
            }
            "closed" => fs::write(
                artifacts.join("closed.json"),
                b"{\"attempt_id\":\"foreign-attempt\"}",
            )
            .unwrap(),
            _ => (),
        }
        let calls = std::cell::Cell::new(0);
        let result = state.seal_campaigns_fixture(&fixture.0, &results, &plan, |context| {
            calls.set(calls.get() + 1);
            assert_eq!(context.recovery.project_root, fixture.0);
            assert_ne!(
                context.recovery.reference.preparation_id,
                context.summary["task"]["attempt_id"].as_str().unwrap()
            );
            let closed: Value = serde_json::from_slice(
                &fs::read(context.recovery.reference.artifacts.join("closed.json")).unwrap(),
            )
            .unwrap();
            if closed["attempt_id"] != context.summary["task"]["attempt_id"] {
                return Err(crate::Error::Input(
                    "fixture retained evaluator belongs to another attempt".into(),
                ));
            }
            Ok(())
        });
        if matches!(case, "valid" | "alias") {
            result.unwrap();
            assert_eq!(calls.get(), 4);
            assert!(
                results
                    .join(".checkpoint/campaign-completed/000001/000002.json")
                    .is_file()
            );
        } else {
            assert!(result.is_err(), "{case}");
            assert!(
                !results.join(".checkpoint/campaign-completed").exists(),
                "{case}"
            );
        }
    }
}

#[test]
fn record_mode_without_typed_track_proof_is_recovery_required_not_reusable() {
    for explicit_null in [false, true] {
        let (fixture, plan, results, state) = fixture_mode(crate::tape::Mode::Record);
        if explicit_null {
            for layout in plan.read_campaigns(&results).unwrap() {
                for slot in &layout.plan.slots {
                    let reference_path = layout
                        .root
                        .join(format!("batch-{}/outcomes/1.json", slot.batch));
                    let mut reference: Value =
                        serde_json::from_slice(&fs::read(&reference_path).unwrap()).unwrap();
                    let path = layout.root.join(reference["path"].as_str().unwrap());
                    let mut outcome: Value =
                        serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
                    outcome["recording_track"] = Value::Null;
                    outcome["summary"]["recording"] = Value::Null;
                    let bytes = serde_json::to_vec(&outcome).unwrap();
                    reference["bytes"] = json!(bytes.len());
                    reference["sha256"] = json!(crate::cassette::digest(&bytes));
                    fs::write(path, bytes).unwrap();
                    fs::write(reference_path, serde_json::to_vec(&reference).unwrap()).unwrap();
                }
            }
        }
        assert!(state.seal_campaigns(&fixture.0, &results, &plan).is_err());
        assert!(!results.join(".checkpoint/campaign-completed").exists());
        for configuration in 0..2 {
            for slot in 1..=2 {
                assert!(
                    results
                        .join(format!(
                            ".checkpoint/campaign-unsettled/{configuration:06}/{slot:06}.json"
                        ))
                        .is_file()
                );
            }
        }
    }
}

#[test]
fn native_unexpected_allocation_is_checked_instead_of_silently_ignored() {
    let (fixture, plan, results, state) = fixture();
    let layouts = plan.read_campaigns(&results).unwrap();
    let path = layouts[1].root.join("batch-2/allocations/1.json");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, b"{}").unwrap();
    assert!(state.seal_campaigns(&fixture.0, &results, &plan).is_err());
    assert!(!results.join(".checkpoint/campaign-completed").exists());
    assert_eq!(fs::read(path).unwrap(), b"{}");
}

#[test]
fn native_false_grades_seal_without_invented_remote_allocations_or_replacement() {
    let (fixture, plan, results, state) = fixture();
    state.seal_campaigns(&fixture.0, &results, &plan).unwrap();
    for configuration in 0..2 {
        for slot in 1..=2 {
            let path = results.join(format!(
                ".checkpoint/campaign-completed/{configuration:06}/{slot:06}.json"
            ));
            assert!(path.is_file());
        }
    }
    let path = results.join(".checkpoint/campaign-completed/000000/000001.json");
    let original = fs::read(&path).unwrap();
    assert!(state.seal_campaigns(&fixture.0, &results, &plan).is_err());
    assert_eq!(fs::read(path).unwrap(), original);
}

#[test]
fn existing_campaign_receipts_verify_exact_original_identity_and_derived_evidence() {
    for case in [
        "valid",
        "schema",
        "scope",
        "configuration",
        "campaign",
        "batch",
        "slot",
        "seed",
        "input",
        "roots",
        "fingerprint",
        "log",
        "missing",
        "symlink",
        "directory-symlink",
        "root-symlink",
        "oversized",
        "unsealed",
    ] {
        let (fixture, plan, results, state) = fixture();
        state.seal_campaigns(&fixture.0, &results, &plan).unwrap();
        let path = results.join(".checkpoint/campaign-completed/000001/000002.json");
        let original = fs::read(&path).unwrap();
        let earlier = results.join(".checkpoint/campaign-completed/000000/000001.json");
        let earlier_bytes = fs::read(&earlier).unwrap();
        let mut receipt: Value = serde_json::from_slice(&original).unwrap();
        let layouts = plan.read_campaigns(&results).unwrap();
        match case {
            "schema" => receipt["schema_version"] = json!(2),
            "scope" => receipt["scope"] = json!("other"),
            "configuration" => receipt["configuration"] = json!(0),
            "campaign" => receipt["execution"]["campaign"] = json!("foreign"),
            "batch" => receipt["execution"]["batch"] = json!(1),
            "slot" => receipt["execution"]["slot"] = json!(1),
            "seed" => receipt["execution"]["replay_seed"] = json!(999),
            "input" => receipt["input_digest"] = json!("f".repeat(64)),
            "roots" => receipt["roots"][0]["path"] = json!(fixture.0.join("foreign")),
            "fingerprint" => receipt["fingerprint"]["digest"] = json!("f".repeat(64)),
            "log" => fs::write(layouts[1].root.join("batch-2/run.jsonl"), "changed").unwrap(),
            "missing" => fs::remove_file(&path).unwrap(),
            "symlink" => {
                fs::remove_file(&path).unwrap();
                std::os::unix::fs::symlink("absent", &path).unwrap();
            }
            "directory-symlink" | "root-symlink" => {
                let directory = if case == "root-symlink" {
                    results.join(".checkpoint/campaign-completed")
                } else {
                    path.parent().unwrap().to_path_buf()
                };
                let target = fixture.0.join("aliased-receipts");
                fs::rename(&directory, &target).unwrap();
                std::os::unix::fs::symlink(&target, &directory).unwrap();
            }
            "oversized" => fs::OpenOptions::new()
                .write(true)
                .open(&path)
                .unwrap()
                .set_len(1024 * 1024 + 1)
                .unwrap(),
            "unsealed" => {
                fs::remove_file(layouts[1].root.join("batch-2/outcomes/1.json")).unwrap();
            }
            _ => (),
        }
        if !matches!(
            case,
            "log"
                | "missing"
                | "symlink"
                | "directory-symlink"
                | "root-symlink"
                | "oversized"
                | "unsealed"
        ) {
            fs::write(&path, serde_json::to_vec(&receipt).unwrap()).unwrap();
        }
        let before = fs::read(results.join("plan.json")).unwrap();
        let result = state.resume_campaign_outcomes(&fixture.0, &results, &plan);
        if case == "valid" {
            let reused = result.unwrap();
            assert_eq!(reused.groups.len(), 2);
            for summaries in reused.groups.values() {
                assert_eq!(summaries.len(), 2);
                assert!(
                    summaries
                        .iter()
                        .all(|summary| summary["workload_success"] == false)
                );
            }
            for layout in &layouts {
                assert!(
                    crate::output_lock::OutputLock::acquire_existing(
                        &layout.root.join(".owner.lock"),
                        "held through reuse"
                    )
                    .is_err()
                );
            }
            drop(reused);
            for layout in &layouts {
                assert!(
                    crate::output_lock::OutputLock::acquire_existing(
                        &layout.root.join(".owner.lock"),
                        "released after reuse"
                    )
                    .is_ok()
                );
            }
        } else {
            assert!(result.is_err(), "{case}");
        }
        assert_eq!(
            fs::read(results.join("plan.json")).unwrap(),
            before,
            "{case}"
        );
        assert!(
            !results.join(".checkpoint/campaign-unsettled").exists(),
            "{case}"
        );
        assert_eq!(fs::read(earlier).unwrap(), earlier_bytes, "{case}");
    }
}

#[test]
fn foreign_prepared_or_start_identity_in_later_configuration_seals_nothing() {
    for field in [
        "agent",
        "provider",
        "model",
        "workload",
        "task",
        "dataset_sha256",
        "protocol_sha256",
        "cohort",
        "execution",
    ] {
        let (fixture, plan, results, state) = fixture();
        let layouts = plan.read_campaigns(&results).unwrap();
        let path = layouts[1]
            .root
            .join(if matches!(field, "cohort" | "execution") {
                "batch-2/starts/1.json"
            } else {
                "batch-2/prepared.json"
            });
        let mut value: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        match field {
            "cohort" => value["execution_cohort"]["id"] = json!("ffff"),
            "execution" => value["execution"]["batch"] = json!(1),
            "dataset_sha256" | "protocol_sha256" => {
                value["compatibility"][field] = json!("f".repeat(64))
            }
            _ => value["compatibility"][field] = json!("foreign"),
        }
        fs::write(path, serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(
            state.seal_campaigns(&fixture.0, &results, &plan).is_err(),
            "{field}"
        );
        assert!(
            !results.join(".checkpoint/campaign-completed").exists(),
            "{field}"
        );
    }
}

#[test]
fn shared_actual_read_budget_applies_across_campaigns_before_retaining_values() {
    let (_fixture, plan, results, _state) = fixture();
    let mut layouts = plan.read_campaigns(&results).unwrap();
    let relative = Path::new("batch-1/prepared.json");
    let size = fs::metadata(layouts[0].root.join(relative)).unwrap().len();
    let budget = std::rc::Rc::new(std::cell::Cell::new(size));
    for layout in &mut layouts {
        layout.share_observation_budget(budget.clone());
    }
    layouts[0].declaration(relative, 65536).unwrap();
    assert_eq!(budget.get(), 0);
    assert!(layouts[1].declaration(relative, 65536).is_err());
}
