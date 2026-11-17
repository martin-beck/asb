//! Real publisher-owner death over synthetic settled history, no agent/provider.
use super::*;
use crate::sweep::checkpoint::{Fingerprint, Root, Snapshotter};
use crate::test_support::IsolatedEnvironment;
use serde::{Deserialize, Serialize};
use std::io::Read;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::process::Command;
use std::time::{Duration, Instant};

#[path = "generation_recording_death_tests.rs"]
mod recording;

const QUALIFIER: &str = "ASB_CONTINUATION_DEATH_QUALIFIER";
const PREFIX: &str = "sweep::checkpoint::campaigns::settled::continuation::generation::tests::resolution::owner_death::";
const WIRE_LIMIT: u64 = 1024 * 1024;

/// Test data only. The child must prove the fingerprint, classify and acquire
/// all original owners; no Prepared or worker capability is deserialized.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Setup {
    parent: PathBuf,
    results: PathBuf,
    roots: Vec<Root>,
    context: Value,
    initial: Fingerprint,
    ready: PathBuf,
    stage: String,
}

/// Keep evidence until the shared deadline confirms whole-tree quiescence.
/// An uncertain cleanup preserves both fixtures instead of deleting active data.
struct PublicationCustody {
    owner: crate::process_gate::ManagedGate,
    fixture: Option<Fixture>,
    control: Option<Fixture>,
    deadline: Instant,
}

impl Drop for PublicationCustody {
    fn drop(&mut self) {
        if self.owner.stop_until(self.deadline).is_err() {
            for fixture in [&mut self.fixture, &mut self.control] {
                if let Some(fixture) = fixture.take() {
                    eprintln!(
                        "uncertain test-owner cleanup; preserved {}",
                        fixture.0.display()
                    );
                    std::mem::forget(fixture);
                }
            }
        }
    }
}

fn command(test: &str) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    configure(&mut command, test);
    command
}

fn configure(command: &mut Command, test: &str) {
    let executable = std::env::current_exe().unwrap();
    let runtime = executable.parent().unwrap().parent().unwrap().join("asb");
    command
        .isolated_env()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("ASB_RUNTIME_EXECUTABLE", runtime)
        .env(QUALIFIER, "1")
        .args(["--exact", &format!("{PREFIX}{test}"), "--nocapture"]);
}

#[test]
fn publisher_child() {
    if std::env::var_os(QUALIFIER).is_none() {
        return;
    }
    let mut bytes = Vec::new();
    std::io::stdin()
        .lock()
        .take(WIRE_LIMIT + 1)
        .read_to_end(&mut bytes)
        .unwrap();
    assert!(bytes.len() as u64 <= WIRE_LIMIT);
    let setup: Setup = serde_json::from_slice(&bytes).unwrap();
    assert!(matches!(setup.stage.as_str(), "admission" | "link"));
    let sweep: SweepPlan = serde_json::from_slice(
        &reading::bounded(&setup.results.join("plan.json"), MAX_GENERATION as u64).unwrap(),
    )
    .unwrap();
    let directory = setup.results.join(".checkpoint");
    let mut inputs = Inputs {
        snapshotter: Snapshotter::open(&directory, "python3".into()).unwrap(),
        directory,
        roots: setup.roots,
        context: setup.context,
        initial: setup.initial,
        stability: None,
        runtime: None,
    };
    assert_eq!(
        inputs.final_fingerprint().unwrap(),
        inputs.initial,
        "publisher must use the exact original execution closure/environment"
    );
    let classified = inputs
        .classify_campaigns(&setup.parent, &setup.results, &sweep)
        .unwrap();
    let prepared =
        Prepared::plan(inputs, classified, &setup.parent, &setup.results, &sweep).unwrap();
    let child = prepared.guard.inputs.directory.join(&prepared.relative);
    let predecessor = prepared
        .predecessor
        .as_ref()
        .unwrap()
        .head_root()
        .to_path_buf();
    let mut reached = false;
    let result = prepared.publish_with_confirmation(
        &setup.results,
        &sweep,
        |_| Ok(()),
        |path| {
            let admission = child.join("admitted.json").is_file();
            let linked = predecessor.join("next.json").is_file();
            let selected = if setup.stage == "admission" {
                path == child && admission && !linked
            } else {
                path == predecessor && admission && linked
            };
            if selected {
                reached = true;
                crate::runner::repeated::publish_bytes(
                    &setup.ready,
                    &serde_json::to_vec(&json!({"child":child,"stage":setup.stage})).unwrap(),
                )?;
                // The parent deliberately terminates this actual publication owner.
                // There are no active snapshot helpers at this synchronous hook.
                let deadline = Instant::now() + Duration::from_secs(45);
                while Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(20));
                }
                return Err(Error::Io(
                    "owner-death qualifier parent did not terminate publisher".into(),
                ));
            }
            Ok(())
        },
    );
    panic!(
        "publisher unexpectedly returned: reached={reached}, error={:?}",
        result.err()
    );
}

#[test]
fn actual_owner_death_preserves_unlinked_and_linked_generations_for_zero_client_repair() {
    if std::env::var_os(QUALIFIER).is_none() {
        let result = command(
            "actual_owner_death_preserves_unlinked_and_linked_generations_for_zero_client_repair",
        )
        .output()
        .unwrap();
        assert!(
            result.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        return;
    }
    for stage in ["admission", "link"] {
        let (fixture, sweep, results, inputs) = successor_inputs();
        let control = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/continuation-death-controls")
            .join(crate::workloads::TaskIdentity::local("death-control", None).attempt_id);
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(control.parent().unwrap())
            .unwrap();
        fs::DirBuilder::new().mode(0o700).create(&control).unwrap();
        let control = Fixture(control.canonicalize().unwrap());
        let layouts = sweep.read_campaigns(&results).unwrap();
        let mut frozen = vec![(
            results.join("plan.json"),
            fs::read(results.join("plan.json")).unwrap(),
        )];
        for name in [
            "inputs.json",
            "campaign-continuation/admitted.json",
            "campaign-continuation/000000.json",
            "campaign-continuation/000001.json",
        ] {
            let path = inputs.directory.join(name);
            frozen.push((path.clone(), fs::read(path).unwrap()));
        }
        for (configuration, layout) in layouts.iter().enumerate() {
            for slot in &layout.plan.slots {
                let path = inputs.campaign_completion_path(configuration, slot.id);
                if path.exists() {
                    frozen.push((path.clone(), fs::read(path).unwrap()));
                }
            }
        }
        let ready = control.0.join("ready.json");
        let setup = Setup {
            parent: fixture.0.clone(),
            results: results.clone(),
            roots: inputs.roots.clone(),
            context: inputs.context.clone(),
            initial: inputs.initial.clone(),
            ready: ready.clone(),
            stage: stage.into(),
        };
        let bytes = serde_json::to_vec(&setup).unwrap();
        assert!(bytes.len() as u64 <= WIRE_LIMIT);
        let setup_path = control.0.join("setup.json");
        crate::runner::repeated::publish_bytes(&setup_path, &bytes).unwrap();
        // A regular-file stdin provides an already complete bounded frame and
        // EOF. No blocking pipe writer or background I/O thread is introduced.
        let executable = std::env::current_exe().unwrap();
        let runtime = executable.parent().unwrap().parent().unwrap().join("asb");
        let launch = crate::launch::ProcessLaunch::new(&executable);
        let (mut child_command, route) = launch.native(Some(&runtime));
        configure(&mut child_command, "publisher_child");
        let output = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(control.0.join("child.output"))
            .unwrap();
        child_command
            .stdin(fs::File::open(setup_path).unwrap())
            .stdout(output.try_clone().unwrap())
            .stderr(output);
        let owner =
            crate::process_gate::Admission::new(route.validate_gate(&mut child_command).unwrap())
                .unwrap()
                .spawn()
                .unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut custody = PublicationCustody {
            owner,
            fixture: Some(fixture),
            control: Some(control),
            deadline,
        };
        let fixture = custody.fixture.as_ref().unwrap();
        let control = custody.control.as_ref().unwrap();
        let owner = &mut custody.owner;
        assert!(owner.resume_when(|| Instant::now() < deadline).unwrap());
        while !ready.is_file() {
            assert!(
                owner.try_wait().unwrap().is_none(),
                "publisher exited before {stage}: {}",
                String::from_utf8_lossy(
                    &reading::bounded(&control.0.join("child.output"), 64 * 1024).unwrap()
                )
            );
            assert!(Instant::now() < deadline, "publisher did not reach {stage}");
            std::thread::sleep(Duration::from_millis(10));
        }
        let frame: Value =
            serde_json::from_slice(&reading::bounded(&ready, 4096).unwrap()).unwrap();
        assert_eq!(frame["stage"], stage);
        let child = PathBuf::from(frame["child"].as_str().unwrap());
        assert_eq!(child, inputs.directory.join(ROOT).join("successors/000001"));
        let admission = fs::read(child.join("admitted.json")).unwrap();
        let manifests: Vec<_> = (0..2)
            .map(|index| fs::read(child.join(manifest_path(index))).unwrap())
            .collect();
        for layout in &layouts {
            assert!(
                crate::output_lock::OutputLock::acquire_existing(
                    &layout.root.join(".owner.lock"),
                    "test busy"
                )
                .is_err()
            );
        }
        assert!(Inputs::recover_continuation(&fixture.0, &results, &sweep).is_err());
        // Guardian cancellation kills the blocked owner without Rust unwinding,
        // then confirms descendant quiescence. This is real signal death, not
        // a simulated Result error and not a claim of a particular signal.
        let status = owner.stop_until(deadline).unwrap();
        assert!(!status.success());
        for layout in &layouts {
            drop(
                crate::output_lock::OutputLock::acquire_existing(
                    &layout.root.join(".owner.lock"),
                    "test reacquire",
                )
                .unwrap(),
            );
        }
        for iteration in 0..2 {
            let report = Inputs::recover_continuation(&fixture.0, &results, &sweep).unwrap();
            assert_eq!(report["linked"], stage == "admission" && iteration == 0);
            assert_eq!(report["agent_executions"], 0);
            assert_eq!(report["evaluations"], 0);
            assert_eq!(fs::read(child.join("admitted.json")).unwrap(), admission);
            for (index, bytes) in manifests.iter().enumerate() {
                assert_eq!(fs::read(child.join(manifest_path(index))).unwrap(), *bytes);
            }
            for (path, bytes) in &frozen {
                assert_eq!(fs::read(path).unwrap(), *bytes);
            }
            assert!(!layouts[0].root.join("batch-1/starts/4.json").exists());
            assert!(
                !layouts[0]
                    .root
                    .join(&layouts[0].plan.slots[3].workspace)
                    .exists()
            );
        }
        let classified = inputs
            .classify_campaigns(&fixture.0, &results, &sweep)
            .unwrap();
        let prepared = Prepared::plan(inputs, classified, &fixture.0, &results, &sweep).unwrap();
        assert!(prepared.reactivating);
        let (_guard, workers) = prepared.publish(&results, &sweep).unwrap();
        assert_eq!(workers[0].manifest.pending[0].cohort.admitted_slots, [4]);
        assert!(workers[1].manifest.pending.is_empty());
        assert_eq!(workers[0].admission_reference, Blob::of(&admission));
        assert!(!layouts[0].root.join("batch-1/starts/4.json").exists());
    }
}
