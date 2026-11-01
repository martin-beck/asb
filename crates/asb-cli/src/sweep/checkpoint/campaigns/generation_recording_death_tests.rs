//! Actual signal death inside imported-union publication, with synthetic history.
//! This is artifact/publication qualification, not a past-client/SDK claim.
use super::*;
use crate::cassette::{
    Archive, Compatibility, ImportedTrack, Limits, Manifest, OriginPlan, RecordingJournal,
    RecordingSource, Statistics, Track,
};

std::thread_local! {
    static BOUNDARY: std::cell::RefCell<Option<(String, PathBuf)>> = const { std::cell::RefCell::new(None) };
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RecordingSetup {
    destination: PathBuf,
    source: PathBuf,
    ready: PathBuf,
    point: String,
}

fn boundary(point: &str) {
    let ready = BOUNDARY.with_borrow(|boundary| {
        boundary
            .as_ref()
            .and_then(|(selected, ready)| (selected == point).then(|| ready.clone()))
    });
    if let Some(ready) = ready {
        crate::runner::repeated::publish_bytes(&ready, br#"{"inside_publication":true}"#).unwrap();
        let deadline = Instant::now() + Duration::from_secs(45);
        while Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("qualifier failed to terminate the blocked publication owner");
    }
}

fn identity() -> Compatibility {
    Compatibility {
        agent: "pi".into(),
        provider: "local".into(),
        model: "fixture".into(),
        workload: "bug-fix".into(),
        task: "bug-fix".into(),
        dataset_sha256: None,
        prompt_sha256: crate::cassette::digest(b"prompt"),
        material_sha256: crate::cassette::digest(b"material"),
        protocol_sha256: None,
    }
}

fn manifest(id: &str, provenance: Option<OriginPlan>) -> Manifest {
    Manifest {
        schema_version: 1,
        recording_id: id.into(),
        compatibility: identity(),
        concurrency: 1,
        tracks: Vec::new(),
        provenance,
    }
}

fn track(id: u64) -> Track {
    Track {
        recorder: None,
        id,
        batch: id,
        replica: 1,
        complete: true,
        process_success: Some(false),
        workload_success: Some(false),
        bytes: 3,
        sha256: crate::cassette::digest(b"{}\n"),
        statistics: Statistics::default(),
        native: None,
        artifacts: Vec::new(),
    }
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
    let setup: RecordingSetup = serde_json::from_slice(&bytes).unwrap();
    let mut source = Archive::open(&setup.source, Limits::default()).unwrap();
    let old = source.manifest().tracks[0].clone();
    let origins = OriginPlan {
        schema_version: 1,
        population: 2,
        sources: vec![RecordingSource {
            recording_id: source.manifest().recording_id.clone(),
            manifest_sha256: crate::cassette::digest(
                &serde_json::to_vec(source.manifest()).unwrap(),
            ),
        }],
        imports: vec![ImportedTrack {
            slot: old.id,
            source: 0,
            metadata_sha256: crate::cassette::track_metadata_digest(&old).unwrap(),
            bytes: old.bytes,
        }],
    };
    let mut journal = RecordingJournal::create(
        &setup.destination,
        manifest("new-union", Some(origins)),
        2,
        Limits::default(),
    )
    .unwrap();
    BOUNDARY.with_borrow_mut(|boundary| *boundary = Some((setup.point.clone(), setup.ready)));
    journal.observe_publication(boundary);
    // No execution intent is manufactured for this historical import.
    journal
        .import_track(old, 0, &mut source.read_track(1).unwrap().as_slice())
        .unwrap();
    if setup.point == "imports-complete" {
        boundary("imports-complete");
    }
    // Local attempt facts are synthetic too. A failed complete trace remains
    // complete; publication must not reinterpret failure as an incomplete tape.
    journal.begin(2, 1).unwrap();
    journal.commit(track(2), &mut &b"{}\n"[..]).unwrap();
    journal.finalize().unwrap();
    panic!("publisher returned without reaching selected boundary");
}

#[test]
fn actual_imported_union_owner_death_retains_exact_payloads_and_recovers_idempotently() {
    if std::env::var_os(QUALIFIER).is_none() {
        let output = command("recording::actual_imported_union_owner_death_retains_exact_payloads_and_recovers_idempotently").output().unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    for point in [
        "before-payload-commit",
        "committed-1.json:before-write",
        "imports-complete",
        "after-final-link",
        "after-published",
    ] {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/recording-death-controls")
            .join(crate::workloads::TaskIdentity::local("recording-death", None).attempt_id);
        fs::create_dir_all(&root).unwrap();
        let fixture = Fixture(root.canonicalize().unwrap());
        let source = fixture.0.join("source.zip");
        let mut historical = RecordingJournal::create(
            &source,
            manifest("historical-source", None),
            1,
            Limits::default(),
        )
        .unwrap();
        historical.begin(1, 1).unwrap();
        historical.commit(track(1), &mut &b"{}\n"[..]).unwrap();
        historical.finalize().unwrap();
        drop(historical);
        let original = fs::read(&source).unwrap();
        let destination = fixture.0.join("union.zip");
        let ready = fixture.0.join("ready.json");
        let setup = RecordingSetup {
            destination: destination.clone(),
            source: source.clone(),
            ready: ready.clone(),
            point: point.into(),
        };
        let setup_path = fixture.0.join("setup.json");
        crate::runner::repeated::publish_bytes(&setup_path, &serde_json::to_vec(&setup).unwrap())
            .unwrap();
        let executable = std::env::current_exe().unwrap();
        let runtime = executable.parent().unwrap().parent().unwrap().join("asb");
        let launch = crate::launch::ProcessLaunch::new(&executable);
        let (mut child, route) = launch.native(Some(&runtime));
        configure(&mut child, "recording::publisher_child");
        let output = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(fixture.0.join("child.output"))
            .unwrap();
        child
            .stdin(fs::File::open(setup_path).unwrap())
            .stdout(output.try_clone().unwrap())
            .stderr(output);
        let owner = crate::process_gate::Admission::new(route.validate_gate(&mut child).unwrap())
            .unwrap()
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut custody = PublicationCustody {
            owner,
            fixture: Some(fixture),
            control: None,
            deadline,
        };
        let root = custody.fixture.as_ref().unwrap().0.clone();
        assert!(
            custody
                .owner
                .resume_when(|| Instant::now() < deadline)
                .unwrap()
        );
        while !ready.is_file() {
            assert!(
                custody.owner.try_wait().unwrap().is_none(),
                "publisher exited before {point}: {}",
                String::from_utf8_lossy(
                    &reading::bounded(&root.join("child.output"), 64 * 1024).unwrap()
                )
            );
            assert!(Instant::now() < deadline, "publisher did not reach {point}");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            serde_json::from_slice::<Value>(&fs::read(&ready).unwrap()).unwrap()["inside_publication"],
            true
        );
        let busy = RecordingJournal::recover(&destination, Limits::default())
            .err()
            .map(|error| error.to_string());
        assert!(
            busy.is_some_and(|error| error.contains("another writer")),
            "live writer exclusion, not an unrelated recovery failure"
        );
        let published_before = fs::read(&destination).ok();
        assert!(!custody.owner.stop_until(deadline).unwrap().success());
        let mut recovered = RecordingJournal::recover(&destination, Limits::default()).unwrap();
        if matches!(
            point,
            "before-payload-commit" | "committed-1.json:before-write"
        ) {
            assert!(
                recovered.imported_only_snapshot().is_err(),
                "durable bytes without commit are not admitted imports"
            );
            let partial = recovered.partial_directory().to_path_buf();
            assert_eq!(fs::read(partial.join("track-1.jsonl")).unwrap(), b"{}\n");
            assert!(!partial.join("committed-1.json").exists());
            recovered
                .resume_import(track(1), 0, &mut &b"{}\n"[..])
                .unwrap();
        }
        if point == "committed-1.json:before-write" {
            // Death while allocating metadata also leaves an orphan pending
            // file. Even a recovered exact import must not silently adopt that
            // envelope as an unused admitted writer or delete unknown bytes.
            let pending = fs::read_dir(recovered.partial_directory())
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .filter(|path| {
                    path.file_name()
                        .unwrap()
                        .to_string_lossy()
                        .starts_with(".pending-")
                })
                .map(|path| {
                    let bytes = fs::read(&path).unwrap();
                    (path, bytes)
                })
                .collect::<Vec<_>>();
            assert_eq!(pending.len(), 1);
            for _ in 0..2 {
                assert!(recovered.imported_only_snapshot().is_err());
                for (path, bytes) in &pending {
                    assert_eq!(fs::read(path).unwrap(), *bytes);
                }
                drop(recovered);
                recovered = RecordingJournal::recover(&destination, Limits::default()).unwrap();
            }
            assert!(!destination.exists());
            assert!(
                !recovered
                    .partial_directory()
                    .join("started-2.json")
                    .exists()
            );
            assert_eq!(fs::read(&source).unwrap(), original);
            continue;
        }
        if matches!(point, "before-payload-commit" | "imports-complete") {
            assert_eq!(
                recovered.imported_only_snapshot().unwrap().tracks,
                vec![track(1)]
            );
            assert!(
                !recovered
                    .partial_directory()
                    .join("started-2.json")
                    .exists()
            );
        }
        let first = recovered.finalize().unwrap();
        let bytes = fs::read(&destination).unwrap();
        if let Some(prior) = published_before {
            assert_eq!(bytes, prior, "recover exact linked archive");
        }
        assert!(first.cleanup_complete);
        drop(recovered);
        let mut recovered = RecordingJournal::recover(&destination, Limits::default()).unwrap();
        let second = recovered.finalize().unwrap();
        assert_eq!(first.manifest, second.manifest);
        assert_eq!(fs::read(&destination).unwrap(), bytes);
        assert_eq!(fs::read(&source).unwrap(), original);
        let mut archive = Archive::open(&destination, Limits::default()).unwrap();
        assert_eq!(archive.manifest().tracks[0], track(1));
        assert_eq!(archive.read_track(1).unwrap(), b"{}\n");
        assert!(
            archive
                .manifest()
                .tracks
                .iter()
                .all(|track| track.complete && track.workload_success == Some(false))
        );
        // No client, provider or evaluator command exists in this harness.
        assert_eq!(
            archive.manifest().tracks.len(),
            if matches!(point, "after-final-link" | "after-published") {
                2
            } else {
                1
            }
        );
    }
}
