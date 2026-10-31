use super::*;
use crate::sweep::{Options, SweepFile, campaigns, plan};
use std::{fs, path::PathBuf};

#[path = "reservations_tests.rs"]
pub(in crate::sweep::checkpoint) mod reservations_tests;

#[path = "settlement_tests.rs"]
pub(in crate::sweep::checkpoint) mod settlement_tests;

pub(in crate::sweep::checkpoint) struct Fixture(pub(in crate::sweep::checkpoint) PathBuf);
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

pub(in crate::sweep::checkpoint) fn published(
    mode: crate::tape::Mode,
) -> (Fixture, SweepPlan, PathBuf) {
    published_concurrency(mode, 1)
}

pub(in crate::sweep::checkpoint) fn published_concurrency(
    mode: crate::tape::Mode,
    concurrency: usize,
) -> (Fixture, SweepPlan, PathBuf) {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/campaign-seal-fixtures")
        .join(crate::workloads::TaskIdentity::local("inputs", None).attempt_id);
    fs::create_dir_all(root.join(".asb")).unwrap();
    let root = root.canonicalize().unwrap();
    fs::write(
        root.join(".asb/project.toml"),
        toml::to_string(&crate::project::ProjectConfig::default()).unwrap(),
    )
    .unwrap();
    let mut refs = Vec::new();
    let tapes = root.join("tapes");
    for index in 0..2 {
        let reference = format!("benchmark-{index}.toml");
        fs::write(
            root.join(&reference),
            format!("agent='pi'\nprovider='local'\nmodel='fixture'\nworkload='bug-fix'\nconcurrency={concurrency}\n"),
        )
        .unwrap();
        let directory = tapes.join(format!("{index:03}-benchmark-{index}/cassettes"));
        fs::create_dir_all(&directory).unwrap();
        fs::write(
            directory.join("benchmark-pi-bug-fix-fixture.jsonl"),
            b"{}\n",
        )
        .unwrap();
        refs.push(reference);
    }
    let mut plan = plan::build(
        &root,
        &SweepFile {
            repeat: None,
            configs: refs,
            resources: None,
        },
    )
    .unwrap();
    plan.execution_mode = match mode {
        crate::tape::Mode::Record => plan::SavedExecutionMode::Record,
        crate::tape::Mode::Replay => plan::SavedExecutionMode::Replay,
    };
    let results = root.join("sweep-results");
    let prepared = campaigns::prepare(
        &root,
        &results,
        &root.join("history"),
        &mut plan,
        Options {
            mode: Some(mode),
            repeat: Some(2),
            cassette_root: Some(&tapes),
            ..Default::default()
        },
    )
    .unwrap()
    .unwrap();
    plan::fresh_results(&root, "sweep", &plan).unwrap();
    let pool = prepared.publish(&results).unwrap();
    drop(pool); // No provider, agent, environment or attempt has been started.
    (Fixture(root), plan, results)
}

#[test]
fn campaign_seal_roots_bind_immutable_inputs_not_outputs_or_original_replay_paths() {
    for mode in [crate::tape::Mode::Record, crate::tape::Mode::Replay] {
        let (fixture, plan, results) = published(mode);
        let roots = input_roots(&results, &plan).unwrap();
        let layouts = plan.read_campaigns(&results).unwrap();
        assert_eq!(
            roots.len(),
            if mode == crate::tape::Mode::Replay {
                6
            } else {
                4
            }
        );
        let snapshotter =
            super::super::Snapshotter::open(&results.join(".checkpoint"), "python3".into())
                .unwrap();
        let fingerprint = || {
            snapshotter
                .fingerprint(
                    &roots,
                    &serde_json::json!({"scope":"input-root-test-only"}),
                    false,
                    &[],
                )
                .unwrap()
        };
        let before = fingerprint();
        for layout in &layouts {
            let batch = layout.root.join("batch-1");
            fs::create_dir_all(batch.join("workspace.asb-agent-runtime")).unwrap();
            fs::write(batch.join("run.jsonl"), "new output").unwrap();
            fs::write(
                batch.join("workspace.asb-agent-runtime/session"),
                "new session",
            )
            .unwrap();
            if let Some(recording) = &layout.plan.recording {
                fs::write(recording, "growing recording output").unwrap();
            } else {
                fs::write(
                    layout.plan.replay_source.as_ref().unwrap(),
                    "original source replaced after private staging",
                )
                .unwrap();
            }
        }
        assert_eq!(fingerprint(), before);
        if mode == crate::tape::Mode::Replay {
            let input = layouts[1]
                .root
                .join(&layouts[1].plan.replay_inputs[0].private);
            fs::write(&input, "changed private bytes").unwrap();
            assert_ne!(fingerprint(), before);
            fs::write(input, b"{}\n").unwrap();
            assert_eq!(fingerprint(), before);
        }
        fs::write(fixture.0.join("benchmark-1.toml"), "changed configuration").unwrap();
        assert_ne!(fingerprint(), before);
    }
}

#[test]
fn later_invalid_campaign_reference_cannot_become_an_input_declaration() {
    let (_fixture, plan, results) = published(crate::tape::Mode::Replay);
    let layouts = plan.read_campaigns(&results).unwrap();
    let later = layouts[1].root.join("plan.json");
    let original = fs::read(&later).unwrap();
    fs::write(&later, b"malformed later plan").unwrap();
    assert!(input_roots(&results, &plan).is_err());
    assert!(!results.join(".checkpoint").exists());
    assert!(!layouts[0].root.join("batch-1").exists());
    fs::write(later, original).unwrap();
    assert!(input_roots(&results, &plan).is_ok());
}

#[test]
fn continuation_private_inputs_match_original_declarations_without_source_archives() {
    let (_fixture, plan, results) = published(crate::tape::Mode::Replay);
    let layouts = plan.read_campaigns(&results).unwrap();
    for layout in &layouts {
        fs::remove_file(layout.plan.replay_source.as_ref().unwrap()).unwrap();
    }
    verify_private_inputs(&results, &plan).unwrap();
    let paths: Vec<_> = layouts
        .iter()
        .map(|layout| layout.root.join(&layout.plan.replay_inputs[0].private))
        .collect();
    // Matching length alone, including bytes sealed after staging corruption,
    // cannot stand in for the original declared selected-track checksum.
    fs::write(&paths[0], b"[]\n").unwrap();
    assert!(
        verify_private_inputs(&results, &plan)
            .unwrap_err()
            .to_string()
            .contains("bytes differ")
    );
    // The whole metadata population is checked before the first payload hash.
    fs::write(&paths[1], b"too long\n").unwrap();
    assert!(
        verify_private_inputs(&results, &plan)
            .unwrap_err()
            .to_string()
            .contains("length differs")
    );
    fs::write(&paths[0], b"{}\n").unwrap();
    fs::write(&paths[1], b"{}\n").unwrap();
    verify_private_inputs(&results, &plan).unwrap();
    for layout in &layouts {
        assert!(!layout.root.join("batch-1").exists());
    }
    assert!(!results.join(".checkpoint").exists());
}

#[test]
fn continuation_private_input_aliases_are_rejected_without_modifying_targets() {
    use std::os::unix::fs::symlink;
    let (fixture, plan, results) = published(crate::tape::Mode::Replay);
    let layouts = plan.read_campaigns(&results).unwrap();
    let input = layouts[1]
        .root
        .join(&layouts[1].plan.replay_inputs[0].private);
    let foreign = fixture.0.join("foreign.jsonl");
    fs::write(&foreign, b"{}\n").unwrap();
    fs::remove_file(&input).unwrap();
    symlink(&foreign, &input).unwrap();
    assert!(verify_private_inputs(&results, &plan).is_err());
    assert_eq!(fs::read(&foreign).unwrap(), b"{}\n");
    fs::remove_file(&input).unwrap();
    fs::write(&input, b"{}\n").unwrap();
    let parent = input.parent().unwrap();
    let retained = parent.with_extension("retained");
    fs::rename(parent, &retained).unwrap();
    symlink(&retained, parent).unwrap();
    assert!(verify_private_inputs(&results, &plan).is_err());
    assert_eq!(
        fs::read(retained.join(input.file_name().unwrap())).unwrap(),
        b"{}\n"
    );
    assert!(!results.join(".checkpoint").exists());
}
