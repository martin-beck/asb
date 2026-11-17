use super::*;
use crate::sweep::checkpoint::{Inputs, Snapshotter};
use serde_json::{Value, json};

pub(in crate::sweep::checkpoint) fn inputs(results: &Path, plan: &SweepPlan) -> Inputs {
    let directory = results.join(".checkpoint");
    let roots = input_roots(results, plan).unwrap();
    let snapshotter = Snapshotter::open(&directory, "python3".into()).unwrap();
    let context = json!({"scope":"reservation-fixture-not-full-runtime-qualification"});
    let initial = snapshotter
        .fingerprint(&roots, &context, false, &[])
        .unwrap();
    Inputs {
        directory,
        snapshotter,
        roots,
        context,
        initial,
        stability: None,
        runtime: None,
    }
}

fn manifest(results: &Path, index: usize) -> PathBuf {
    results
        .join(".checkpoint/campaign-reservations")
        .join(format!("{index:06}.json"))
}

#[test]
fn compact_reservations_reload_all_original_campaigns_without_starting_slots() {
    for mode in [crate::tape::Mode::Record, crate::tape::Mode::Replay] {
        let (_fixture, plan, results) = published(mode);
        let state = inputs(&results, &plan);
        state.publish("pending", None).unwrap();
        assert!(state.verify_campaign_reservations(&results, &plan).is_err());
        state.reserve_campaigns(&results, &plan).unwrap();
        let seal = fs::read(results.join(".checkpoint/inputs.json")).unwrap();
        for (index, layout) in plan.read_campaigns(&results).unwrap().iter().enumerate() {
            let value: Value =
                serde_json::from_slice(&fs::read(manifest(&results, index)).unwrap()).unwrap();
            assert_eq!(value["configuration"], index);
            assert_eq!(value["campaign"], layout.plan.campaign);
            assert_eq!(value["repeat"], 2);
            assert_eq!(value["concurrency"], 1);
            assert_eq!(value["slots"], 2);
            assert_eq!(value["input_digest"], state.initial.digest);
            assert_eq!(
                value["reference"],
                serde_json::to_value(plan.entries[index].campaign.as_ref().unwrap()).unwrap()
            );
            assert!(!layout.root.join("batch-1").exists());
            assert!(!layout.root.join("batch-2").exists());
        }
        drop(state);
        let reloaded = inputs(&results, &plan);
        reloaded
            .verify_campaign_reservations(&results, &plan)
            .unwrap();
        let marker = results.join(".checkpoint/campaign-admission.json");
        let original = fs::read(&marker).unwrap();
        assert!(reloaded.reserve_campaigns(&results, &plan).is_err());
        assert_eq!(fs::read(marker).unwrap(), original);
        assert_eq!(
            fs::read(results.join(".checkpoint/inputs.json")).unwrap(),
            seal
        );
    }
}

#[test]
fn reservation_publication_faults_retain_evidence_without_admission_or_replacement() {
    for blocked in ["first", "last", "marker"] {
        let (_fixture, plan, results) = published(crate::tape::Mode::Replay);
        let state = inputs(&results, &plan);
        state.publish("pending", None).unwrap();
        let seal_path = results.join(".checkpoint/inputs.json");
        let seal = fs::read(&seal_path).unwrap();
        let blocked_path = match blocked {
            "first" => manifest(&results, 0),
            "last" => manifest(&results, 1),
            _ => results.join(".checkpoint/campaign-admission.json"),
        };
        fs::create_dir_all(blocked_path.parent().unwrap()).unwrap();
        fs::write(&blocked_path, b"foreign sentinel").unwrap();
        assert!(
            state.reserve_campaigns(&results, &plan).is_err(),
            "{blocked}"
        );
        assert_eq!(fs::read(&blocked_path).unwrap(), b"foreign sentinel");
        assert_eq!(fs::read(&seal_path).unwrap(), seal);
        assert!(state.verify_campaign_reservations(&results, &plan).is_err());
        if blocked != "first" {
            assert!(manifest(&results, 0).is_file());
        }
        if blocked == "marker" {
            assert!(manifest(&results, 1).is_file());
        } else {
            assert!(!results.join(".checkpoint/campaign-admission.json").exists());
        }
        for layout in plan.read_campaigns(&results).unwrap() {
            assert!(!layout.root.join("batch-1").exists());
        }
    }
}

#[test]
fn reservations_bind_all_manifest_fields_and_exact_population_before_reuse() {
    let (_fixture, plan, results) = published(crate::tape::Mode::Replay);
    let state = inputs(&results, &plan);
    state.publish("pending", None).unwrap();
    state.reserve_campaigns(&results, &plan).unwrap();
    let path = manifest(&results, 1);
    let original = fs::read(&path).unwrap();
    let permissions = fs::metadata(&path).unwrap().permissions();
    let value: Value = serde_json::from_slice(&original).unwrap();
    for field in [
        "schema_version",
        "scope",
        "run_id",
        "input_digest",
        "configuration",
        "campaign",
        "reference",
        "repeat",
        "concurrency",
        "slots",
    ] {
        let mut changed = value.clone();
        changed.as_object_mut().unwrap().remove(field);
        fs::write(&path, serde_json::to_vec_pretty(&changed).unwrap()).unwrap();
        let marker = fs::read(results.join(".checkpoint/campaign-admission.json")).unwrap();
        assert!(
            state.verify_campaign_reservations(&results, &plan).is_err(),
            "{field}"
        );
        assert_eq!(
            fs::read(results.join(".checkpoint/campaign-admission.json")).unwrap(),
            marker
        );
    }
    fs::write(&path, fs::read(manifest(&results, 0)).unwrap()).unwrap();
    assert!(state.verify_campaign_reservations(&results, &plan).is_err());
    fs::remove_file(&path).unwrap();
    assert!(state.verify_campaign_reservations(&results, &plan).is_err());
    assert!(!path.exists()); // Verification never fabricates missing reservations.
    fs::write(&path, &original).unwrap();
    fs::set_permissions(&path, permissions).unwrap();
    state.verify_campaign_reservations(&results, &plan).unwrap();
    let mut changed = plan;
    changed.entries.swap(0, 1);
    assert!(
        state
            .verify_campaign_reservations(&results, &changed)
            .is_err()
    );
}

#[test]
fn corrupt_or_aliased_reservation_markers_never_authorize_continuation() {
    use std::os::unix::fs::symlink;
    let (fixture, plan, results) = published(crate::tape::Mode::Replay);
    let state = inputs(&results, &plan);
    state.publish("pending", None).unwrap();
    state.reserve_campaigns(&results, &plan).unwrap();
    let path = results.join(".checkpoint/campaign-admission.json");
    let original = fs::read(&path).unwrap();
    let value: Value = serde_json::from_slice(&original).unwrap();
    for field in [
        "schema_version",
        "scope",
        "run_id",
        "input_digest",
        "manifests",
        "fingerprint",
    ] {
        let mut changed = value.clone();
        changed[field] = Value::Null;
        fs::write(&path, serde_json::to_vec_pretty(&changed).unwrap()).unwrap();
        assert!(
            state.verify_campaign_reservations(&results, &plan).is_err(),
            "{field}"
        );
    }
    fs::write(&path, &original).unwrap();
    let input = manifest(&results, 0);
    let retained = fixture.0.join("retained-reservation.json");
    fs::rename(&input, &retained).unwrap();
    symlink(&retained, &input).unwrap();
    assert!(state.verify_campaign_reservations(&results, &plan).is_err());
    fs::remove_file(&input).unwrap();
    fs::rename(&retained, &input).unwrap();
    let directory = input.parent().unwrap();
    let retained_dir = fixture.0.join("retained-reservations");
    fs::rename(directory, &retained_dir).unwrap();
    symlink(&retained_dir, directory).unwrap();
    assert!(state.verify_campaign_reservations(&results, &plan).is_err());
}
