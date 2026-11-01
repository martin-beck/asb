use super::*;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use zip::{CompressionMethod, ZipWriter, write::SimpleFileOptions};

#[path = "proof_tests.rs"]
mod proof;
#[path = "provenance_tests.rs"]
mod provenance_tests;
#[path = "publication_tests.rs"]
mod publication;
#[path = "reservation_tests.rs"]
mod reservation;

#[test]
fn campaign_inspection_can_select_more_than_256_tracks_from_one_descriptor() {
    let fixture = Fixture::new();
    fs::create_dir(fixture.0.join(".asb")).unwrap();
    fs::write(
        fixture.0.join(".asb/project.toml"),
        toml::to_string(&crate::project::ProjectConfig::default()).unwrap(),
    )
    .unwrap();
    let configuration: crate::benchmark::BenchmarkConfig =
        toml::from_str("agent='pi'\nprovider='local'\nmodel='fixture'\nworkload='bug-fix'\n")
            .unwrap();
    let path = fixture.0.join("benchmark.toml");
    fs::write(&path, toml::to_string(&configuration).unwrap()).unwrap();
    let names = (1..=257)
        .map(|id| format!("tracks/{id}.jsonl"))
        .collect::<Vec<_>>();
    let payloads = names
        .iter()
        .map(|name| (name.as_str(), b"{}\n".as_slice()))
        .collect::<Vec<_>>();
    let bundle = fixture.zip(&manifest(257), &payloads);
    // This is planning/metadata qualification, not material compatibility or a
    // live replay. A deterministic sample visits all tracks before publication.
    let inspection = crate::runner::repeated::Inspection::prepare_selected(
        &path,
        configuration,
        4000,
        crate::runner::RunOptions {
            mode: Some(crate::tape::Mode::Replay),
            cassette: Some(bundle.to_str().unwrap()),
            replay_seed: Some(42),
            ..Default::default()
        },
        None,
        None,
    )
    .unwrap();
    let cost = inspection.input_cost().unwrap();
    assert_eq!(cost.inputs, 257);
    assert_eq!(cost.files, 1);
    assert_eq!(cost.bytes, 257 * 3);
    assert_eq!(cost.observed_bytes, 0);
    assert!(
        crate::runner::repeated::InputCost::default()
            .checked_add(cost)
            .is_ok()
    );
    inspection.validate_sources().unwrap();
    let prepared = inspection.observe().unwrap();
    assert!(!fixture.0.join("results").exists());
    let expected = manifest(257).compatibility;
    let published = crate::runner::repeated::publish_campaign(prepared).unwrap();
    drop(published);
    // Publication starts no material gate/client. Move away the original ZIP
    // before a first batch could observe material: its complete expected
    // identity must already survive in the checksummed plan/private inputs.
    fs::rename(&bundle, fixture.0.join("original-bundle-moved-away.zip")).unwrap();
    let campaigns = fixture.0.join("results/asb-executions");
    let root = fs::read_dir(campaigns)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let restored = crate::runner::campaign::reading::Layout::read(&root.join("plan.json")).unwrap();
    restored
        .plan
        .replay_compatibility
        .as_ref()
        .unwrap()
        .require_match(&expected)
        .unwrap();
    assert!(!root.join("batch-1").exists());
    assert!(
        restored
            .plan
            .replay_inputs
            .iter()
            .all(|input| root.join(&input.private).is_file())
    );
}

#[test]
fn selected_recorder_identity_survives_archive_staging_and_saved_plan() {
    let fixture = Fixture::new();
    fs::create_dir(fixture.0.join(".asb")).unwrap();
    fs::write(
        fixture.0.join(".asb/project.toml"),
        toml::to_string(&crate::project::ProjectConfig::default()).unwrap(),
    )
    .unwrap();
    let config: crate::benchmark::BenchmarkConfig =
        toml::from_str("agent='pi'\nprovider='local'\nmodel='fixture'\nworkload='bug-fix'\n")
            .unwrap();
    let path = fixture.0.join("benchmark.toml");
    fs::write(&path, toml::to_string(&config).unwrap()).unwrap();
    let mut source = manifest(1);
    source.tracks[0].recorder = Some("other-recorder".into());
    let bundle = fixture.zip(&source, &[("tracks/1.jsonl", b"{}\n")]);
    let inspection = crate::runner::repeated::Inspection::prepare_selected(
        &path,
        config,
        1,
        crate::runner::RunOptions {
            mode: Some(crate::tape::Mode::Replay),
            cassette: Some(bundle.to_str().unwrap()),
            replay_seed: Some(42),
            ..Default::default()
        },
        None,
        None,
    )
    .unwrap();
    let prepared = inspection.observe().unwrap();
    drop(crate::runner::repeated::publish_campaign(prepared).unwrap());
    let root = fs::read_dir(fixture.0.join("results/asb-executions"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let restored = crate::runner::campaign::reading::Layout::read(&root.join("plan.json")).unwrap();
    assert_eq!(
        restored.plan.replay_inputs[0].recorder.as_deref(),
        Some("other-recorder")
    );
    assert_eq!(
        fs::read(root.join(&restored.plan.replay_inputs[0].private)).unwrap(),
        b"{}\n"
    );
}

#[cfg(unix)]
#[test]
fn alternate_recorder_runs_an_agent_offline_and_reports_its_metrics() {
    use std::os::unix::fs::PermissionsExt;
    if std::env::var_os("ASB_SYNTHETIC_RECORDER_CHILD").is_none() {
        let runtime = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/debug/asb");
        assert!(
            runtime.is_file(),
            "build the actual ASB binary before this test"
        );
        let provider = crate::recording_provider_fixture::RecordingProvider::new();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "cassette::tests::alternate_recorder_runs_an_agent_offline_and_reports_its_metrics",
                "--nocapture",
            ])
            .env("ASB_SYNTHETIC_RECORDER_CHILD", "1")
            .env("ASB_PROVIDER_BASE_URL", &provider.endpoint)
            .env("OPENROUTER_BASE_URL", format!("{}/v1", provider.endpoint))
            .env(
                "OPENROUTER_MODELS_URL",
                format!("{}/models", provider.endpoint),
            )
            .env("OPENROUTER_API_KEY", "fixture-only-key")
            .env("ASB_RUNTIME_EXECUTABLE", runtime.canonicalize().unwrap())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "child failed: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            provider.calls.load(Ordering::SeqCst),
            0,
            "synthetic recorder must not contact a provider for inference"
        );
        return;
    }
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.0.join(".asb")).unwrap();
    fs::write(
        fixture.0.join(".asb/project.toml"),
        "tools_dir='tools'\nworkloads_dir='workloads'\nworkspace_dir='workspace'\n",
    )
    .unwrap();
    fs::create_dir_all(fixture.0.join("workloads")).unwrap();
    fs::write(
        fixture.0.join("workloads/bug-fix.txt"),
        "Repair src/parser.py so parse_path ignores empty components.",
    )
    .unwrap();
    fs::create_dir_all(fixture.0.join("workspace")).unwrap();
    let agent = fixture.0.join("fixture-agent");
    fs::write(&agent, r##"#!/usr/bin/python3
import os, pathlib, urllib.request
url = os.environ['ASB_PROVIDER_BASE_URL'].rstrip('/')
if url.endswith('/v1'): url = url[:-3]
req = urllib.request.Request(url + '/v1/chat/completions', data=b'{"model":"fixture","messages":[]}', headers={'Content-Type':'application/json'})
with urllib.request.urlopen(req, timeout=5) as response:
    assert response.status == 200
    response.read()
pathlib.Path('src/parser.py').write_text("def parse_path(value: str) -> list[str]:\n    return [segment for segment in value.split('/') if segment]\n")
"##).unwrap();
    fs::set_permissions(&agent, fs::Permissions::from_mode(0o700)).unwrap();
    let config = fixture.0.join("benchmark.toml");
    fs::write(&config, format!("agent='{}'\nprovider='openrouter'\nmodel='fixture'\nworkload='bug-fix'\nworkspace='workspace'\nlog_file='run.log'\n", agent.display())).unwrap();
    let cassette = fixture.0.join("synthetic.zip");
    let _capacity = crate::disk_space::test_available_mib(u64::MAX);
    let mut recorded = Vec::new();
    let record = crate::runner::run_with_tape(
        config.to_str().unwrap(),
        crate::runner::RunOptions {
            mode: Some(crate::tape::Mode::Record),
            cassette: Some(cassette.to_str().unwrap()),
            json: true,
            ..Default::default()
        },
        &mut recorded,
    );
    assert!(
        record.is_ok(),
        "record={record:?} output={}",
        String::from_utf8_lossy(&recorded)
    );
    let archive = crate::cassette::Source::open(&cassette, Limits::default()).unwrap();
    assert_eq!(
        archive.manifest().unwrap().tracks[0].recorder.as_deref(),
        Some("synthetic-recorder")
    );
    drop(archive);
    let mut output = Vec::new();
    let result = crate::runner::run_with_tape(
        config.to_str().unwrap(),
        crate::runner::RunOptions {
            mode: Some(crate::tape::Mode::Replay),
            cassette: Some(cassette.to_str().unwrap()),
            tool_mode_explicit: true,
            json: true,
            ..Default::default()
        },
        &mut output,
    );
    assert!(
        result.is_ok(),
        "replay={result:?} output={}",
        String::from_utf8_lossy(&output)
    );
    let summary = String::from_utf8_lossy(&output)
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|row| row.get("process_success").is_some())
        .expect("replay summary");
    assert_eq!(summary["process_success"], true);
    assert_eq!(summary["token_usage"]["input_tokens"], 11);
    assert_eq!(summary["token_usage"]["output_tokens"], 7);
    assert_eq!(summary["token_usage"]["cached_tokens"], 2);
    assert_eq!(summary["token_usage"]["total_tokens"], 18);
    assert_eq!(summary["tool_call_count"], 1);
}

#[cfg(unix)]
#[test]
fn bundle_inspection_decodes_the_probed_descriptor_not_a_replacement_path() {
    let fixture = Fixture::new();
    let original = fixture.0.join("original.zip");
    fs::rename(
        fixture.zip(&manifest(1), &[("tracks/1.jsonl", b"{}\n")]),
        &original,
    )
    .unwrap();
    let replacement = fixture.zip(
        &manifest(2),
        &[("tracks/1.jsonl", b"{}\n"), ("tracks/2.jsonl", b"{}\n")],
    );
    let replacement_bytes = fs::read(&replacement).unwrap();
    super::source::ADMISSION_REPLACEMENT
        .with(|value| *value.borrow_mut() = Some((replacement, original.clone())));
    let mut source = Source::inspect(&original, Limits::default())
        .unwrap()
        .observe()
        .unwrap();
    assert_eq!(source.manifest().unwrap().tracks.len(), 1);
    assert_eq!(source.read_track(1).unwrap(), b"{}\n");
    assert!(source.read_track(2).is_err());
    assert_eq!(fs::read(original).unwrap(), replacement_bytes);
    super::source::ADMISSION_REPLACEMENT.with(|value| assert!(value.borrow().is_none()));
}

#[test]
fn source_inspection_pins_metadata_without_observing_payloads() {
    let fixture = Fixture::new();
    let base = fixture.0.join("inspection.jsonl");
    for id in [2, 64] {
        fs::write(crate::benchmark_paths::replica_path(&base, id), b"{}\n").unwrap();
    }
    super::source::take_observation_count();
    let inspection = Source::inspect(&base, Limits::default()).unwrap();
    assert_eq!(super::source::take_observation_count(), 0);
    // Path replacement cannot redirect the later observation to different bytes.
    let original = crate::benchmark_paths::replica_path(&base, 64);
    fs::rename(&original, fixture.0.join("retained-original")).unwrap();
    fs::write(original, b"replacement\n").unwrap();
    let mut observed = inspection.observe().unwrap();
    assert_eq!(super::source::take_observation_count(), 2);
    assert_eq!(observed.read_track(64).unwrap(), b"{}\n");
}

#[test]
fn source_inspection_checks_all_pinned_lengths_before_first_hash() {
    for length in [0, 4] {
        let fixture = Fixture::new();
        let base = fixture.0.join("inspection.jsonl");
        for id in [2, 64] {
            fs::write(crate::benchmark_paths::replica_path(&base, id), b"{}\n").unwrap();
        }
        let inspection = Source::inspect(&base, Limits::default()).unwrap();
        File::options()
            .write(true)
            .open(crate::benchmark_paths::replica_path(&base, 64))
            .unwrap()
            .set_len(length)
            .unwrap();
        super::source::take_observation_count();
        assert!(
            inspection
                .observe()
                .err()
                .unwrap()
                .to_string()
                .contains("length changed after inspection")
        );
        assert_eq!(super::source::take_observation_count(), 0);
    }
}

#[test]
fn late_source_inspection_error_needs_no_earlier_payload_observation() {
    let fixture = Fixture::new();
    let source = fixture.0.join("first.jsonl");
    fs::write(&source, b"{}\n").unwrap();
    super::source::take_observation_count();
    let first = Source::inspect(&source, Limits::default()).unwrap();
    let invalid = fixture.0.join("second.jsonl");
    fs::write(&invalid, b"PKnot-a-valid-zip").unwrap();
    assert!(Source::inspect(&invalid, Limits::default()).is_err());
    assert_eq!(super::source::take_observation_count(), 0);
    drop(first);
    assert_eq!(fs::read(source).unwrap(), b"{}\n");
}

#[test]
fn source_detection_uses_content_and_preserves_unknown_legacy_metadata() {
    let fixture = Fixture::new();
    let archive = fixture.zip(&manifest(1), &[("tracks/1.jsonl", b"{}\n")]);
    let renamed = fixture.0.join("actually-a-bundle.jsonl");
    fs::rename(archive, &renamed).unwrap();
    let mut source = Source::open(&renamed, Limits::default()).unwrap();
    assert!(source.manifest().is_some());
    assert!(source.legacy_tracks().is_empty());
    assert_eq!(source.read_track(1).unwrap(), b"{}\n");
    let legacy = fixture.0.join("actually-jsonl.zip");
    fs::write(&legacy, b"{}\n").unwrap();
    let mut source = Source::open(&legacy, Limits::default()).unwrap();
    assert!(source.manifest().is_none());
    assert_eq!(source.legacy_tracks()[0].id, 1);
    assert_eq!(source.read_track(1).unwrap(), b"{}\n");
    assert!(source.read_track(2).is_err());
    fs::write(fixture.0.join("corrupt.jsonl"), b"PKbad archive").unwrap();
    assert!(Source::open(&fixture.0.join("corrupt.jsonl"), Limits::default()).is_err());
}

#[test]
fn sparse_legacy_sources_include_replica_64_and_single_base_takes_precedence() {
    let fixture = Fixture::new();
    let base = fixture.0.join("input.jsonl");
    for id in [2, 64] {
        fs::write(
            crate::benchmark_paths::replica_path(&base, id),
            format!("{{\"id\":{id}}}\n"),
        )
        .unwrap();
    }
    let mut source = Source::open(&base, Limits::default()).unwrap();
    assert_eq!(
        source
            .legacy_tracks()
            .iter()
            .map(|track| track.id)
            .collect::<Vec<_>>(),
        [2, 64]
    );
    assert!(source.read_track(1).is_err());
    assert_eq!(source.read_track(64).unwrap(), b"{\"id\":64}\n");
    fs::write(&base, b"{}\n").unwrap();
    let mut single = Source::open(&base, Limits::default()).unwrap();
    assert_eq!(single.legacy_tracks().len(), 1);
    assert_eq!(single.read_track(1).unwrap(), b"{}\n");
    assert!(single.read_track(64).is_err());
}

#[test]
fn legacy_source_limits_and_frozen_checksums_are_enforced() {
    super::source::take_observation_count();
    let fixture = Fixture::new();
    let base = fixture.0.join("input.jsonl");
    for id in [2, 64] {
        fs::write(crate::benchmark_paths::replica_path(&base, id), b"{}\n").unwrap();
    }
    for limits in [
        Limits {
            max_tracks: 1,
            ..Limits::default()
        },
        Limits {
            max_track_bytes: 2,
            ..Limits::default()
        },
        Limits {
            max_total_bytes: 5,
            ..Limits::default()
        },
    ] {
        assert!(Source::open(&base, limits).is_err());
        assert_eq!(
            super::source::take_observation_count(),
            0,
            "over-budget layouts must not hash payloads"
        );
    }
    let mut source = Source::open(&base, Limits::default()).unwrap();
    assert_eq!(super::source::take_observation_count(), 2);
    fs::write(crate::benchmark_paths::replica_path(&base, 2), b"[]\n").unwrap();
    assert!(
        source
            .read_track(2)
            .unwrap_err()
            .to_string()
            .contains("changed")
    );
    assert_eq!(source.read_track(64).unwrap(), b"{}\n");
    fs::write(
        crate::benchmark_paths::replica_path(&base, 64),
        b"{}\nextra",
    )
    .unwrap();
    assert!(source.read_track(64).is_err());
    assert!(Source::open(&fixture.0.join("missing"), Limits::default()).is_err());
    assert!(Source::open(&fixture.0, Limits::default()).is_err());
}

#[cfg(unix)]
#[test]
fn legacy_source_rejects_fifo_and_dangling_symlinks_without_falling_back() {
    use std::os::unix::fs::symlink;
    let fixture = Fixture::new();
    let base = fixture.0.join("input.jsonl");
    fs::write(crate::benchmark_paths::replica_path(&base, 64), b"{}\n").unwrap();
    symlink(fixture.0.join("missing"), &base).unwrap();
    assert!(Source::open(&base, Limits::default()).is_err());
    let fifo = fixture.0.join("pipe.jsonl");
    assert!(
        std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap()
            .success()
    );
    assert!(Source::open(&fifo, Limits::default()).is_err());
}

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/cassette-tests")
            .join(format!(
                "{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn zip(&self, manifest: &Manifest, payloads: &[(&str, &[u8])]) -> PathBuf {
        self.zip_with_method(manifest, payloads, CompressionMethod::Deflated)
    }

    fn zip_with_method(
        &self,
        manifest: &Manifest,
        payloads: &[(&str, &[u8])],
        method: CompressionMethod,
    ) -> PathBuf {
        let path = self.0.join("fixture.zip");
        let mut writer = ZipWriter::new(File::create(&path).unwrap());
        let options = SimpleFileOptions::default().compression_method(method);
        writer.start_file("manifest.json", options).unwrap();
        writer
            .write_all(&serde_json::to_vec(manifest).unwrap())
            .unwrap();
        for (name, bytes) in payloads {
            writer.start_file(*name, options).unwrap();
            writer.write_all(bytes).unwrap();
        }
        writer.finish().unwrap();
        path
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // Positively-owned per-test directory, never a shared workspace root.
        fs::remove_dir_all(&self.0).unwrap();
    }
}

fn manifest(count: u64) -> Manifest {
    Manifest {
        provenance: None,
        schema_version: SCHEMA_VERSION,
        recording_id: "fixture-recording-1".into(),
        compatibility: Compatibility {
            agent: "fixture-agent".into(),
            provider: "fixture-provider".into(),
            model: "fixture/model".into(),
            workload: "fixture-workload".into(),
            task: "task-1".into(),
            dataset_sha256: None,
            prompt_sha256: digest(b"prompt"),
            material_sha256: digest(b"initial material"),
            protocol_sha256: None,
        },
        concurrency: 1,
        tracks: (1..=count)
            .map(|id| Track {
                recorder: None,
                id,
                batch: id,
                replica: 1,
                complete: true,
                process_success: Some(true),
                workload_success: Some(true),
                bytes: 3,
                sha256: digest(b"{}\n"),
                statistics: Statistics {
                    tokens: Some(id * 10),
                    duration_ms: Some(id * 100),
                    requests: Some(id),
                    steps: None,
                },
                native: None,
                artifacts: Vec::new(),
            })
            .collect(),
    }
}

#[test]
fn recorder_identity_is_bounded_and_historical_tracks_remain_readable() {
    let mut value = manifest(1);
    let legacy = serde_json::to_value(&value).unwrap();
    assert!(legacy["tracks"][0].get("recorder").is_none());
    assert_eq!(serde_json::from_value::<Manifest>(legacy).unwrap(), value);
    value.tracks[0].recorder = Some("other-recorder".into());
    value.validate(Limits::default()).unwrap();
    for bad in ["", "bad/name", "bad\nname", &"a".repeat(65)] {
        value.tracks[0].recorder = Some(bad.into());
        assert!(value.validate(Limits::default()).is_err());
    }
}

#[test]
fn auxiliary_artifacts_are_sealed_and_read_by_catalogue_identity() {
    let fixture = Fixture::new();
    let mut manifest = manifest(1);
    manifest.tracks[0].artifacts.push(Artifact {
        kind: "local-tools".into(),
        bytes: 16,
        sha256: digest(b"{\"tool\":\"read\"}\n"),
    });
    let archive_path = fixture.zip(
        &manifest,
        &[
            ("tracks/1.jsonl", b"{}\n".as_slice()),
            (
                "artifacts/1/local-tools.jsonl",
                b"{\"tool\":\"read\"}\n".as_slice(),
            ),
        ],
    );
    let mut archive = Archive::open(&archive_path, Limits::default()).unwrap();
    assert_eq!(
        archive.read_artifact(1, "local-tools").unwrap(),
        b"{\"tool\":\"read\"}\n"
    );
    assert!(archive.read_artifact(1, "missing").is_err());
}

#[test]
fn auxiliary_artifacts_reject_unlisted_or_duplicate_paths() {
    let fixture = Fixture::new();
    let mut manifest = manifest(1);
    manifest.tracks[0].artifacts.push(Artifact {
        kind: "local-tools".into(),
        bytes: 3,
        sha256: digest(b"{}\n"),
    });
    let unlisted = fixture.zip(
        &manifest,
        &[
            ("tracks/1.jsonl", b"{}\n".as_slice()),
            ("artifacts/1/local-tools.jsonl", b"{}\n".as_slice()),
            ("artifacts/1/foreign.jsonl", b"{}\n".as_slice()),
        ],
    );
    assert!(Archive::open(&unlisted, Limits::default()).is_err());
    manifest.tracks[0].artifacts.push(Artifact {
        kind: "local-tools".into(),
        bytes: 3,
        sha256: digest(b"{}\n"),
    });
    assert!(manifest.validate(Limits::default()).is_err());
    let mut one_artifact = manifest.clone();
    one_artifact.tracks[0].artifacts.truncate(1);
    assert!(
        one_artifact
            .validate(Limits {
                max_tracks: 1,
                ..Limits::default()
            })
            .is_err()
    );
}

#[test]
fn journal_publishes_auxiliary_artifacts_with_the_track() {
    let fixture = Fixture::new();
    let destination = fixture.0.join("recording.zip");
    let mut journal =
        RecordingJournal::create(&destination, manifest(0), 1, Limits::default()).unwrap();
    journal.begin(1, 1).unwrap();
    let mut provider = b"{}\n".as_slice();
    let mut tools = b"{\"tool\":\"read\"}\n".as_slice();
    journal
        .commit_with_artifacts(
            manifest(1).tracks.remove(0),
            &mut provider,
            vec![ArtifactInput {
                artifact: Artifact {
                    kind: "local-tools".into(),
                    bytes: 16,
                    sha256: digest(b"{\"tool\":\"read\"}\n"),
                },
                source: &mut tools,
            }],
        )
        .unwrap();
    journal.finalize().unwrap();
    let mut archive = Archive::open(&destination, Limits::default()).unwrap();
    assert_eq!(
        archive.read_artifact(1, "local-tools").unwrap(),
        b"{\"tool\":\"read\"}\n"
    );
}

#[test]
fn native_sidecars_are_sealed_complete_and_published_with_the_track() {
    let fixture = Fixture::new();
    let destination = fixture.0.join("recording.zip");
    let mut journal =
        RecordingJournal::create(&destination, manifest(0), 1, Limits::default()).unwrap();
    journal.begin(1, 1).unwrap();
    let native_bytes = b"{\"event\":\"launch\"}\n";
    let native = NativeTrack {
        complete: true,
        events: 1,
        bytes: native_bytes.len() as u64,
        sha256: digest(native_bytes),
    };
    let mut provider = b"{}\n".as_slice();
    let mut source = native_bytes.as_slice();
    journal
        .commit_with_native(
            manifest(1).tracks.remove(0),
            &mut provider,
            Vec::new(),
            Some(NativeInput {
                native: native.clone(),
                source: &mut source,
            }),
        )
        .unwrap();
    journal.finalize().unwrap();
    let mut archive = Archive::open(&destination, Limits::default()).unwrap();
    assert_eq!(archive.manifest().tracks[0].native.as_ref(), Some(&native));
    assert_eq!(archive.read_native(1).unwrap(), native_bytes);
}

#[test]
fn native_transport_consumes_only_a_sealed_archive_sidecar() {
    let fixture = Fixture::new();
    let digest_value = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let request_sha256 = digest(b"request");
    let output_sha256 = digest(b"output");
    let native_bytes = format!(
        "{{\"event\":\"launch\",\"session\":\"launch-1\",\"program_sha256\":\"{digest_value}\"}}\n{{\"event\":\"session_alias\",\"alias\":\"shell-1\",\"session\":\"launch-1\"}}\n{{\"event\":\"poll\",\"alias\":\"shell-1\"}}\n{{\"event\":\"write\",\"alias\":\"shell-1\",\"request_sha256\":\"{request_sha256}\",\"request_b64\":\"cmVxdWVzdA==\"}}\n{{\"event\":\"output\",\"alias\":\"shell-1\",\"response_sha256\":\"{output_sha256}\",\"output_b64\":\"b3V0cHV0\"}}\n{{\"event\":\"exit\",\"alias\":\"shell-1\",\"success\":true}}\n{{\"event\":\"workspace_effect\",\"sha256\":\"{digest_value}\"}}\n"
    );
    let mut manifest = manifest(1);
    manifest.tracks[0].native = Some(NativeTrack {
        complete: true,
        events: 7,
        bytes: native_bytes.len() as u64,
        sha256: digest(native_bytes.as_bytes()),
    });
    let path = fixture.zip(
        &manifest,
        &[
            ("tracks/1.jsonl", b"{}\n".as_slice()),
            ("native/1.jsonl", native_bytes.as_bytes()),
        ],
    );
    let mut archive = Archive::open(&path, Limits::default()).unwrap();
    let transport = NativeToolTransport::from_archive(&mut archive, 1).unwrap();
    assert_eq!(transport.sessions()[0].alias(), "shell-1");
}

#[test]
fn native_sidecars_reject_orphan_duplicate_oversize_incomplete_and_mutated_payloads() {
    let fixture = Fixture::new();
    let native_bytes = b"{\"event\":\"launch\"}\n";
    let native = NativeTrack {
        complete: true,
        events: 1,
        bytes: native_bytes.len() as u64,
        sha256: digest(native_bytes),
    };
    let plain = manifest(1);
    let orphan = fixture.zip(
        &plain,
        &[
            ("tracks/1.jsonl", b"{}\n".as_slice()),
            ("native/1.jsonl", native_bytes.as_slice()),
        ],
    );
    assert!(Archive::open(&orphan, Limits::default()).is_err());

    let mut declared = manifest(1);
    declared.tracks[0].native = Some(native.clone());
    let mut legacy = declared.clone();
    legacy.schema_version = 1;
    assert!(legacy.validate(Limits::default()).is_err());
    let duplicate = fixture.zip(
        &declared,
        &[
            ("tracks/1.jsonl", b"{}\n".as_slice()),
            ("native/1.jsonl", native_bytes.as_slice()),
            ("native/2.jsonl", native_bytes.as_slice()),
        ],
    );
    let mut duplicate_bytes = fs::read(&duplicate).unwrap();
    replace_name(&mut duplicate_bytes, b"native/2.jsonl", b"native/1.jsonl");
    fs::write(&duplicate, duplicate_bytes).unwrap();
    assert!(Archive::open(&duplicate, Limits::default()).is_err());

    let oversize_archive = fixture.zip(
        &declared,
        &[
            ("tracks/1.jsonl", b"{}\n".as_slice()),
            ("native/1.jsonl", native_bytes.as_slice()),
        ],
    );
    assert!(
        Archive::open(
            &oversize_archive,
            Limits {
                max_track_bytes: native_bytes.len() as u64 - 1,
                ..Limits::default()
            },
        )
        .is_err()
    );

    let mut incomplete = declared.clone();
    incomplete.tracks[0].native.as_mut().unwrap().complete = false;
    let incomplete_archive = fixture.zip(
        &incomplete,
        &[
            ("tracks/1.jsonl", b"{}\n".as_slice()),
            ("native/1.jsonl", native_bytes.as_slice()),
        ],
    );
    let mut archive = Archive::open(&incomplete_archive, Limits::default()).unwrap();
    assert!(archive.read_native(1).is_err());

    let mutated = fixture.zip(
        &declared,
        &[
            ("tracks/1.jsonl", b"{}\n".as_slice()),
            ("native/1.jsonl", b"{\"event\":\"broken\"}\n".as_slice()),
        ],
    );
    let mut archive = Archive::open(&mutated, Limits::default()).unwrap();
    assert!(archive.read_native(1).is_err());
}

#[test]
fn journal_never_commits_a_track_when_its_native_sidecar_is_incomplete() {
    let fixture = Fixture::new();
    let destination = fixture.0.join("recording.zip");
    let mut journal =
        RecordingJournal::create(&destination, manifest(0), 1, Limits::default()).unwrap();
    journal.begin(1, 1).unwrap();
    let native_bytes = b"{\"event\":\"launch\"}";
    let native = NativeTrack {
        complete: true,
        events: 1,
        bytes: native_bytes.len() as u64,
        sha256: digest(native_bytes),
    };
    let mut provider = b"{}\n".as_slice();
    let mut source = native_bytes.as_slice();
    assert!(
        journal
            .commit_with_native(
                manifest(1).tracks.remove(0),
                &mut provider,
                Vec::new(),
                Some(NativeInput {
                    native,
                    source: &mut source,
                }),
            )
            .is_err()
    );
    assert!(journal.finalize().is_err());
}

#[test]
fn probe_only_capture_is_inspectable_but_not_automatically_selected() {
    let mut catalogue = manifest(2);
    catalogue.tracks[0].statistics.requests = Some(0);
    catalogue.tracks[1].process_success = Some(false);
    catalogue.tracks[1].workload_success = Some(false);
    let plan = SelectionPlan::build(&catalogue, 5, 2, 42, None, 20).unwrap();
    assert_eq!(plan.tracks, vec![2; 10]);
    assert_eq!(Selector::Id(1).resolve(&catalogue).unwrap(), 1);
    assert_eq!(
        "min:requests"
            .parse::<Selector>()
            .unwrap()
            .resolve(&catalogue)
            .unwrap(),
        2
    );
    catalogue.tracks[1].statistics.requests = Some(0);
    assert!(SelectionPlan::build(&catalogue, 1, 1, 42, None, 20).is_err());
    catalogue.tracks[0].statistics.requests = None;
    assert_eq!(
        SelectionPlan::build(&catalogue, 1, 1, 42, None, 20)
            .unwrap()
            .tracks,
        vec![1]
    );
}

#[test]
fn schema_identity_bounds_order_and_checked_slot_mapping() {
    let limits = Limits::default();
    let original = manifest(3);
    original.validate(limits).unwrap();
    assert_eq!(limits.max_tracks, 65_533);
    assert!(
        original
            .validate(Limits {
                max_tracks: 65_534,
                ..limits
            })
            .is_err()
    );
    let mutations: [fn(&mut Manifest); 11] = [
        |m| m.schema_version = SCHEMA_VERSION + 1,
        |m| m.recording_id = "../elsewhere".into(),
        |m| m.concurrency = 65,
        |m| m.compatibility.prompt_sha256 = "G".repeat(64),
        |m| m.compatibility.agent = "\x1b[2J".into(),
        |m| m.tracks.swap(0, 1),
        |m| m.tracks[0].batch = 0,
        |m| m.tracks[0].replica = 2,
        |m| m.tracks[0].id = 0,
        |m| m.tracks[0].sha256 = "not a hash".into(),
        |m| {
            m.concurrency = 64;
            m.tracks[0].batch = u64::MAX;
        },
    ];
    for mutate in mutations {
        let mut invalid = original.clone();
        mutate(&mut invalid);
        assert!(invalid.validate(limits).is_err(), "accepted {invalid:?}");
    }
    for limits in [
        Limits {
            max_tracks: 2,
            ..limits
        },
        Limits {
            max_track_bytes: 2,
            ..limits
        },
        Limits {
            max_total_bytes: 8,
            ..limits
        },
    ] {
        assert!(original.validate(limits).is_err());
    }
    let mut gapped = original.clone();
    gapped.tracks.remove(1);
    gapped.validate(limits).unwrap();
    assert_eq!(gapped.track(3).unwrap().id, 3);
    assert!(gapped.track(2).is_err());
}

#[test]
fn compatibility_covers_every_declared_identity_but_not_runtime_settings() {
    let identity = manifest(1).compatibility;
    identity.require_match(&identity).unwrap();
    let mut json = serde_json::to_value(&identity).unwrap();
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
        let original = json[field].clone();
        json[field] = serde_json::Value::String(if field.ends_with("sha256") {
            digest(b"different")
        } else {
            "different".into()
        });
        let changed: Compatibility = serde_json::from_value(json.clone()).unwrap();
        assert!(identity.require_match(&changed).is_err(), "missed {field}");
        json[field] = original;
    }
}

#[test]
fn protocol_identity_preserves_old_manifest_encoding_but_rejects_semantic_drift() {
    let old = manifest(1).compatibility;
    let encoded = serde_json::to_vec(&old).unwrap();
    assert!(!String::from_utf8_lossy(&encoded).contains("protocol_sha256"));
    let decoded: Compatibility = serde_json::from_slice(&encoded).unwrap();
    assert_eq!(serde_json::to_vec(&decoded).unwrap(), encoded);
    let mut current = old.clone();
    current.protocol_sha256 = Some(digest(b"harness/protocol-v1"));
    current.validate().unwrap();
    assert!(current.require_match(&old).is_err());
    let mut changed = current.clone();
    changed.protocol_sha256 = Some(digest(b"harness/protocol-v2"));
    assert!(current.require_match(&changed).is_err());
    changed.protocol_sha256 = Some("invalid".into());
    assert!(changed.validate().is_err());
}

#[test]
fn selectors_parse_strictly_and_numeric_selection_allows_incomplete() {
    for invalid in [
        "0",
        "-1",
        "",
        "1.0",
        "max:steps",
        "min:Tokens",
        "median:requests:extra",
        "maximum:tokens",
        "18446744073709551616",
    ] {
        assert!(invalid.parse::<Selector>().is_err(), "accepted {invalid}");
    }
    assert_eq!("7".parse::<Selector>().unwrap(), Selector::Id(7));
    assert_eq!(
        "max:duration".parse::<Selector>().unwrap(),
        Selector::Max(Metric::Duration)
    );
    let mut m = manifest(1);
    m.tracks[0].complete = false;
    assert_eq!(Selector::Id(1).resolve(&m).unwrap(), 1);
    assert!(Selector::Id(2).resolve(&m).is_err());
    assert!(Selector::Min(Metric::Tokens).resolve(&m).is_err());
}

#[test]
fn statistics_ties_nulls_and_even_lower_median_are_defined() {
    let mut m = manifest(6);
    for metric in [Metric::Tokens, Metric::Duration, Metric::Requests] {
        assert_eq!(Selector::Min(metric).resolve(&m).unwrap(), 1);
        assert_eq!(Selector::Max(metric).resolve(&m).unwrap(), 6);
        assert_eq!(Selector::Median(metric).resolve(&m).unwrap(), 3);
    }
    m.tracks[5].statistics.tokens = Some(50);
    assert_eq!(Selector::Max(Metric::Tokens).resolve(&m).unwrap(), 5);
    m.tracks[0].statistics.tokens = None;
    m.tracks[1].complete = false;
    m.tracks[2].bytes = 0;
    m.tracks[3].process_success = Some(false);
    m.tracks[3].workload_success = Some(false);
    assert_eq!(Selector::Min(Metric::Tokens).resolve(&m).unwrap(), 4);
    assert_eq!(Selector::Median(Metric::Tokens).resolve(&m).unwrap(), 5);
    for track in &mut m.tracks {
        track.statistics.tokens = None;
    }
    assert!(Selector::Max(Metric::Tokens).resolve(&m).is_err());
    assert_eq!(
        serde_json::to_value(Statistics::default()).unwrap()["tokens"],
        serde_json::Value::Null
    );
}

#[test]
fn sampling_has_a_portable_golden_vector_and_replacement() {
    let m = manifest(3);
    let plan = SelectionPlan::build(&m, 3, 4, 42, None, 100).unwrap();
    assert_eq!(plan.tracks, [3, 1, 2, 2, 1, 3, 3, 1, 2, 3, 3, 2]);
    assert_eq!(plan, SelectionPlan::build(&m, 6, 2, 42, None, 100).unwrap());
    assert_ne!(
        plan.tracks,
        SelectionPlan::build(&m, 3, 4, 43, None, 100)
            .unwrap()
            .tracks
    );
    let saved = serde_json::to_vec(&plan).unwrap();
    assert_eq!(
        serde_json::from_slice::<SelectionPlan>(&saved).unwrap(),
        plan
    );
    for selector in [Selector::Id(2), Selector::Median(Metric::Tokens)] {
        assert_eq!(
            SelectionPlan::build(&m, 5, 4, 1, Some(selector), 20)
                .unwrap()
                .tracks,
            vec![2; 20]
        );
    }
}

#[test]
fn selection_counts_and_eligibility_do_not_depend_on_success() {
    let mut m = manifest(1);
    m.tracks[0].process_success = Some(false);
    m.tracks[0].workload_success = Some(false);
    assert_eq!(
        SelectionPlan::build(&m, 5, 4, 0, None, 20).unwrap().tracks,
        vec![1; 20]
    );
    for (repeat, concurrency, limit) in [
        (0, 1, 100),
        (1, 0, 100),
        (1, 65, 100),
        (u64::MAX, 64, 100),
        (2, 4, 7),
    ] {
        assert!(SelectionPlan::build(&m, repeat, concurrency, 0, None, limit).is_err());
    }
    for incomplete in [true, false] {
        let mut m = m.clone();
        if incomplete {
            m.tracks[0].complete = false;
        } else {
            m.tracks[0].bytes = 0;
        }
        assert!(SelectionPlan::build(&m, 1, 1, 0, None, 10).is_err());
    }
}

#[test]
fn zip_round_trip_inspection_and_only_selected_payload_is_verified() {
    let fixture = Fixture::new();
    let mut m = manifest(2);
    // Deliberately wrong digest in unselected payload: opening/reading track 1
    // must not inflate or verify track 2. Selecting it must fail clearly.
    m.tracks[1].sha256 = digest(b"different bytes");
    let path = fixture.zip(
        &m,
        &[("tracks/1.jsonl", b"{}\n"), ("tracks/2.jsonl", b"{}\n")],
    );
    let mut archive = Archive::open(&path, Limits::default()).unwrap();
    assert_eq!(archive.manifest(), &m);
    assert_eq!(archive.read_track(1).unwrap(), b"{}\n");
    assert!(
        archive
            .read_track(2)
            .unwrap_err()
            .to_string()
            .contains("checksum")
    );
    assert!(archive.read_track(3).is_err());
}

#[test]
fn zip_rejects_bad_catalogue_methods_names_and_lengths() {
    let fixture = Fixture::new();
    let m = manifest(1);
    for payloads in [
        vec![],
        vec![("tracks/2.jsonl", &b"{}\n"[..])],
        vec![("../track.jsonl", &b"{}\n"[..])],
        vec![("tracks/01.jsonl", &b"{}\n"[..])],
        vec![("tracks/1.jsonl", &b"too long"[..])],
    ] {
        let path = fixture.zip(&m, &payloads);
        assert!(Archive::open(&path, Limits::default()).is_err());
    }
    let path = fixture.zip_with_method(
        &m,
        &[("tracks/1.jsonl", b"{}\n")],
        CompressionMethod::Stored,
    );
    assert!(Archive::open(&path, Limits::default()).is_err());
    let mut wrong = m.clone();
    wrong.schema_version = SCHEMA_VERSION + 1;
    let path = fixture.zip(&wrong, &[("tracks/1.jsonl", b"{}\n")]);
    assert!(
        Archive::open(&path, Limits::default())
            .unwrap_err_text()
            .contains("schema")
    );
}

// Avoid requiring Debug for a live archive handle just to inspect test errors.
trait ArchiveResult {
    fn unwrap_err_text(self) -> String;
}
impl ArchiveResult for Result<Archive, Error> {
    fn unwrap_err_text(self) -> String {
        match self {
            Ok(_) => panic!("expected invalid archive"),
            Err(error) => error.to_string(),
        }
    }
}

#[test]
fn zip_limits_truncation_and_directory_preflight() {
    let fixture = Fixture::new();
    let path = fixture.zip(
        &manifest(2),
        &[("tracks/1.jsonl", b"{}\n"), ("tracks/2.jsonl", b"{}\n")],
    );
    let normal = Limits::default();
    for limits in [
        Limits {
            max_archive_bytes: 10,
            ..normal
        },
        Limits {
            max_directory_bytes: 1,
            ..normal
        },
        Limits {
            max_manifest_bytes: 1,
            ..normal
        },
        Limits {
            max_track_bytes: 2,
            ..normal
        },
        Limits {
            max_tracks: 1,
            ..normal
        },
        Limits {
            max_total_bytes: 5,
            ..normal
        },
        Limits {
            max_tracks: 0,
            ..normal
        },
    ] {
        assert!(Archive::open(&path, limits).is_err(), "accepted {limits:?}");
    }
    let original = fs::read(&path).unwrap();
    for length in [0, 21, original.len() / 2, original.len() - 1] {
        fs::write(&path, &original[..length]).unwrap();
        assert!(Archive::open(&path, normal).is_err());
    }
    let mut changed = original;
    let end = changed.len() - 22;
    // A false central-directory size must be rejected before zip metadata reads.
    changed[end + 12..end + 16].copy_from_slice(&u32::MAX.to_le_bytes());
    fs::write(&path, changed).unwrap();
    assert!(Archive::open(&path, normal).is_err());
}

#[test]
fn public_selection_rejects_invalid_manifests_and_unallocatable_counts() {
    let original = manifest(3);
    for field in ["schema", "duplicate", "unsorted", "zero", "compatibility"] {
        let mut invalid = original.clone();
        match field {
            "schema" => invalid.schema_version = SCHEMA_VERSION + 1,
            "duplicate" => invalid.tracks[1] = invalid.tracks[0].clone(),
            "unsorted" => invalid.tracks.swap(0, 2),
            "zero" => invalid.tracks[0].id = 0,
            _ => invalid.compatibility.model.clear(),
        }
        assert!(
            Selector::Id(1).resolve(&invalid).is_err(),
            "accepted {field}"
        );
        assert!(
            SelectionPlan::build(&invalid, 1, 1, 1, None, 100).is_err(),
            "sampled {field}"
        );
    }
    assert!(SelectionPlan::build(&original, u64::MAX, 1, 0, None, usize::MAX).is_err());
    assert!(SelectionPlan::build(&original, 1_000_001, 1, 0, None, usize::MAX).is_err());
}

#[test]
fn persisted_selection_is_bound_to_algorithm_manifest_and_every_slot() {
    let m = manifest(3);
    let original = SelectionPlan::build(&m, 2, 4, 42, None, 100).unwrap();
    original.validate(&m, 2, 4, 100).unwrap();
    assert!(original.validate(&m, 2, 3, 100).is_err());
    let mut other = m.clone();
    other.recording_id = "another-recording".into();
    assert!(original.validate(&other, 2, 4, 100).is_err());
    for mutation in 0..5 {
        let mut plan = original.clone();
        match mutation {
            0 => plan.algorithm = "unknown".into(),
            1 => {
                plan.tracks.pop();
            }
            2 => plan.tracks[0] = 0,
            3 => plan.tracks[0] = 2,
            _ => plan.manifest_sha256 = digest(b"wrong"),
        }
        assert!(plan.validate(&m, 2, 4, 100).is_err(), "accepted {mutation}");
    }
    let mut unknown = serde_json::to_value(original).unwrap();
    unknown["unknown_field"] = true.into();
    assert!(serde_json::from_value::<SelectionPlan>(unknown).is_err());
}

fn replace_name(bytes: &mut [u8], from: &[u8], to: &[u8]) {
    assert_eq!(from.len(), to.len());
    let positions: Vec<_> = bytes
        .windows(from.len())
        .enumerate()
        .filter(|(_, window)| *window == from)
        .map(|(index, _)| index)
        .collect();
    for offset in positions {
        bytes[offset..offset + to.len()].copy_from_slice(to);
    }
}

#[test]
fn inspection_does_not_inflate_corrupt_unselected_deflate_data() {
    let fixture = Fixture::new();
    let path = fixture.zip(
        &manifest(2),
        &[("tracks/1.jsonl", b"{}\n"), ("tracks/2.jsonl", b"{}\n")],
    );
    let mut raw = zip::ZipArchive::new(File::open(&path).unwrap()).unwrap();
    let offset = raw.by_index(2).unwrap().data_start().unwrap() as usize;
    let mut bytes = fs::read(&path).unwrap();
    bytes[offset] = 0xff; // Reserved Deflate block encoding, not just a digest edit.
    fs::write(&path, bytes).unwrap();
    let mut archive = Archive::open(&path, Limits::default()).unwrap();
    assert_eq!(archive.read_track(1).unwrap(), b"{}\n");
    assert!(archive.read_track(2).is_err());
}

#[test]
fn zip_rejects_duplicates_encryption_symlinks_and_lying_inflated_lengths() {
    let fixture = Fixture::new();
    let path = fixture.zip(
        &manifest(2),
        &[("tracks/1.jsonl", b"{}\n"), ("tracks/2.jsonl", b"{}\n")],
    );
    let original = fs::read(&path).unwrap();
    let central: Vec<_> = original
        .windows(4)
        .enumerate()
        .filter(|(_, bytes)| *bytes == b"PK\x01\x02")
        .map(|(index, _)| index)
        .collect();
    for variant in 0..5 {
        let mut bytes = original.clone();
        let track = central[2];
        match variant {
            0 => replace_name(&mut bytes, b"tracks/2.jsonl", b"tracks/1.jsonl"),
            1 => bytes[track + 8] |= 1, // encrypted flag
            2 => {
                // Unix creator and symbolic-link mode in external attributes
                bytes[track + 5] = 3;
                bytes[track + 38..track + 42].copy_from_slice(&(0o120777u32 << 16).to_le_bytes());
            }
            3 => bytes[track + 24..track + 28].copy_from_slice(&2u32.to_le_bytes()),
            _ => bytes[track + 34..track + 36].copy_from_slice(&1u16.to_le_bytes()),
        }
        fs::write(&path, bytes).unwrap();
        assert!(
            Archive::open(&path, Limits::default()).is_err(),
            "accepted variant {variant}"
        );
    }
}

#[test]
fn metadata_view_hides_real_payload_until_validation_and_accepts_comments() {
    let fixture = Fixture::new();
    let path = fixture.zip(&manifest(1), &[("tracks/1.jsonl", b"{}\n")]);
    let (mut reader, phase, _, directory) =
        super::metadata::MetadataReader::new(File::open(&path).unwrap(), Limits::default())
            .unwrap();
    let mut magic = [0; 4];
    reader.read_exact(&mut magic).unwrap();
    assert_eq!(magic, [0; 4]); // Underlying local-header bytes are never exposed.
    reader.seek(SeekFrom::Start(directory)).unwrap();
    reader.read_exact(&mut magic).unwrap();
    assert_eq!(&magic, b"PK\x01\x02");
    phase.store(false, Ordering::Release);
    reader.seek(SeekFrom::Start(0)).unwrap();
    reader.read_exact(&mut magic).unwrap();
    assert_eq!(&magic, b"PK\x03\x04");
    let mut bytes = fs::read(&path).unwrap();
    let length = bytes.len();
    bytes[length - 2..].copy_from_slice(&7u16.to_le_bytes());
    bytes.extend_from_slice(b"comment");
    fs::write(&path, bytes).unwrap();
    Archive::open(&path, Limits::default()).unwrap();
}

#[test]
fn alternate_eocd_and_cross_boundary_headers_cannot_bypass_preflight() {
    let fixture = Fixture::new();
    let path = fixture.zip(&manifest(1), &[("tracks/1.jsonl", b"{}\n")]);
    let original = fs::read(&path).unwrap();
    // A plausible small final EOCD points to the old EOCD, not central headers.
    // A ZIP library may fall back to the original valid archive; ASB must not.
    let mut bytes = original.clone();
    let mut end = original[original.len() - 22..].to_vec();
    end[8..12].copy_from_slice(&[1, 0, 1, 0]);
    end[12..16].copy_from_slice(&22u32.to_le_bytes());
    end[16..20].copy_from_slice(&((original.len() - 22) as u32).to_le_bytes());
    bytes.extend_from_slice(&end);
    fs::write(&path, bytes).unwrap();
    assert!(Archive::open(&path, Limits::default()).is_err());
    let directory = original
        .windows(4)
        .position(|bytes| bytes == b"PK\x01\x02")
        .unwrap();
    for field in [28, 30, 32] {
        let mut bytes = original.clone();
        bytes[directory + field..directory + field + 2].copy_from_slice(&u16::MAX.to_le_bytes());
        fs::write(&path, bytes).unwrap();
        assert!(Archive::open(&path, Limits::default()).is_err());
    }
}

#[cfg(unix)]
#[test]
fn nonregular_archive_paths_are_rejected_without_opening_a_blocking_stream() {
    let fixture = Fixture::new();
    let path = fixture.0.join("stream.zip");
    assert!(
        std::process::Command::new("mkfifo")
            .arg(&path)
            .status()
            .unwrap()
            .success()
    );
    assert!(
        Archive::open(&path, Limits::default())
            .unwrap_err_text()
            .contains("regular file")
    );
    let mut value = serde_json::to_value(manifest(1)).unwrap();
    value["unknown_manifest_field"] = true.into();
    assert!(serde_json::from_value::<Manifest>(value).is_err());
}

#[test]
fn journal_publishes_out_of_order_and_recovers_failed_complete_attempts() {
    let fixture = Fixture::new();
    let destination = fixture.0.join("trials.asb.zip");
    let mut journal =
        RecordingJournal::create(&destination, manifest(0), 3, Limits::default()).unwrap();
    assert_eq!(journal.begin(2, 1).unwrap(), 2);
    assert_eq!(journal.begin(1, 1).unwrap(), 1);
    assert!(journal.begin(1, 1).is_err());
    let mut track = manifest(2).tracks[1].clone();
    track.process_success = Some(false);
    track.workload_success = Some(false);
    journal.commit(track.clone(), &mut &b"{}\n"[..]).unwrap();
    assert!(journal.commit(track, &mut &b"{}\n"[..]).is_err());
    let expected = journal.snapshot().unwrap();
    assert_eq!(
        expected
            .tracks
            .iter()
            .map(|track| track.id)
            .collect::<Vec<_>>(),
        [1, 2]
    );
    assert!(!expected.tracks[0].complete);
    assert_eq!(expected.tracks[0].process_success, None);
    assert!(expected.tracks[1].automatically_selectable());
    assert_eq!(expected.tracks[1].process_success, Some(false));
    // Never-started slot three is not fabricated as an attempted execution.
    assert!(expected.track(3).is_err());
    assert!(RecordingJournal::recover(&destination, Limits::default()).is_err());
    drop(journal);
    let recovered = RecordingJournal::recover(&destination, Limits::default()).unwrap();
    assert_eq!(recovered.snapshot().unwrap(), expected);
    drop(recovered);
    assert_eq!(
        RecordingJournal::recover(&destination, Limits::default())
            .unwrap()
            .snapshot()
            .unwrap(),
        expected
    );
    assert!(
        !destination.exists(),
        "journal must not pretend to publish a ZIP"
    );
}

#[test]
fn immutable_track_evidence_verifies_under_active_writer_and_after_payload_cleanup() {
    let fixture = Fixture::new();
    let destination = fixture.0.join("evidence.asb.zip");
    let mut journal =
        RecordingJournal::create(&destination, manifest(0), 2, Limits::default()).unwrap();
    let tracks = manifest(2).tracks;
    journal.begin(1, 1).unwrap();
    assert!(journal.track_evidence(&tracks[0]).is_err());
    journal
        .commit(tracks[0].clone(), &mut &b"{}\n"[..])
        .unwrap();
    let evidence = journal.track_evidence(&tracks[0]).unwrap();
    evidence.verify(Limits::default()).unwrap();
    assert!(RecordingJournal::recover(&destination, Limits::default()).is_err());
    journal.begin(2, 1).unwrap();
    journal
        .commit(tracks[1].clone(), &mut &b"{}\n"[..])
        .unwrap();
    evidence.verify(Limits::default()).unwrap();
    let published = journal.finalize().unwrap();
    assert!(published.cleanup_complete);
    assert!(!journal.partial_directory().join("track-1.jsonl").exists());
    evidence.verify(Limits::default()).unwrap();
    let mut altered = evidence.clone();
    altered.track.process_success = Some(false);
    assert!(altered.verify(Limits::default()).is_err());
    altered = evidence.clone();
    altered.compatibility.model = "different".into();
    assert!(altered.verify(Limits::default()).is_err());
    // A stale copy cannot make loss of the published target look recoverable.
    fs::write(journal.partial_directory().join("track-1.jsonl"), b"{}\n").unwrap();
    fs::remove_file(&destination).unwrap();
    assert!(evidence.verify(Limits::default()).is_err());
}

#[test]
fn immutable_track_evidence_survives_publication_faults_but_not_commit_tampering() {
    for fault in [
        "after-freeze",
        "after-final-link",
        "before-published",
        "after-published",
        "after-payload-cleanup",
    ] {
        let fixture = Fixture::new();
        let destination = fixture.0.join("fault.asb.zip");
        let mut journal =
            RecordingJournal::create(&destination, manifest(0), 1, Limits::default()).unwrap();
        let track = manifest(1).tracks.remove(0);
        journal.begin(1, 1).unwrap();
        journal.commit(track.clone(), &mut &b"{}\n"[..]).unwrap();
        let evidence = journal.track_evidence(&track).unwrap();
        journal.inject_publication_failure(fault);
        let _publication = journal.finalize();
        evidence.verify(Limits::default()).unwrap();
        let path = journal.partial_directory().join("committed-1.json");
        let original = fs::read(&path).unwrap();
        let mut bad: serde_json::Value = serde_json::from_slice(&original).unwrap();
        bad["track"]["complete"] = serde_json::json!(false);
        fs::write(&path, serde_json::to_vec(&bad).unwrap()).unwrap();
        assert!(evidence.verify(Limits::default()).is_err(), "{fault}");
        fs::write(path, original).unwrap();
        evidence.verify(Limits::default()).unwrap();
    }
}

#[test]
fn journal_rejects_predictable_metadata_exhaustion_without_creating_outputs() {
    for (repeat, concurrency, limit) in [(65_533, 1, 16 * 1024 * 1024), (2, 2, 1024), (1, 1, 1)] {
        let fixture = Fixture::new();
        let destination = fixture.0.join("budget.asb.zip");
        let mut initial = manifest(0);
        initial.concurrency = concurrency;
        let error = RecordingJournal::create(
            &destination,
            initial,
            repeat,
            Limits {
                max_manifest_bytes: limit,
                ..Limits::default()
            },
        )
        .err()
        .expect("known impossible population must fail before reservation");
        assert!(error.to_string().contains("metadata"), "{error}");
        assert_eq!(
            fs::read_dir(&fixture.0).unwrap().count(),
            0,
            "preflight must not create a lock, partial directory or final destination"
        );
    }
}

#[test]
fn journal_metadata_reservation_covers_full_width_statistics_and_outcomes() {
    let fixture = Fixture::new();
    let destination = fixture.0.join("budget.asb.zip");
    let mut initial = manifest(0);
    initial.recording_id = "r".repeat(128);
    initial.concurrency = 2;
    // Discover the exact public admission boundary without duplicating the
    // private record schemas or maintaining a separate JSON-size constant.
    let mut low = 1;
    let mut high = 8192;
    while low < high {
        let mid = low + (high - low) / 2;
        let probe = Fixture::new();
        let admitted = RecordingJournal::create(
            &probe.0.join("budget.asb.zip"),
            initial.clone(),
            2,
            Limits {
                max_manifest_bytes: mid,
                ..Limits::default()
            },
        )
        .is_ok();
        if admitted {
            high = mid;
        } else {
            low = mid + 1;
        }
    }
    assert!(low > 1 && low < 8192);
    let too_small = Limits {
        max_manifest_bytes: low - 1,
        ..Limits::default()
    };
    assert!(RecordingJournal::create(&destination, initial.clone(), 2, too_small).is_err());
    assert_eq!(fs::read_dir(&fixture.0).unwrap().count(), 0);
    let limits = Limits {
        max_manifest_bytes: low,
        ..Limits::default()
    };
    let mut journal = RecordingJournal::create(&destination, initial, 2, limits).unwrap();
    for id in 1..=4 {
        let mut track = manifest(1).tracks.remove(0);
        track.id = id;
        track.batch = (id - 1) / 2 + 1;
        track.replica = (id - 1) % 2 + 1;
        track.complete = false;
        track.process_success = Some(false);
        track.workload_success = Some(false);
        track.statistics = Statistics {
            tokens: Some(u64::MAX),
            duration_ms: Some(u64::MAX),
            requests: Some(u64::MAX),
            steps: Some(u64::MAX),
        };
        journal.begin(track.batch, track.replica).unwrap();
        journal.commit(track, &mut &b"{}\n"[..]).unwrap();
    }
    drop(journal);
    let mut journal = RecordingJournal::recover(&destination, limits).unwrap();
    let published = journal.finalize().unwrap();
    assert!(published.cleanup_complete);
    assert_eq!(
        Archive::open(&destination, Limits::default())
            .unwrap()
            .manifest()
            .tracks
            .len(),
        4
    );
}

#[test]
fn journal_recovery_preserves_pre_reservation_sparse_population() {
    let fixture = Fixture::new();
    let destination = fixture.0.join("legacy-partial.asb.zip");
    let mut journal =
        RecordingJournal::create(&destination, manifest(0), 1, Limits::default()).unwrap();
    journal.begin(1, 1).unwrap();
    journal
        .commit(manifest(1).tracks.remove(0), &mut &b"{}\n"[..])
        .unwrap();
    let intent_path = journal.partial_directory().join("intent.json");
    drop(journal);
    // Model an older writer's large intent with a small actual committed prefix.
    let mut intent: serde_json::Value =
        serde_json::from_slice(&fs::read(&intent_path).unwrap()).unwrap();
    intent["repeat"] = serde_json::json!(40);
    fs::write(intent_path, serde_json::to_vec(&intent).unwrap()).unwrap();
    let limits = Limits {
        max_manifest_bytes: 2000,
        ..Limits::default()
    };
    assert!(
        RecordingJournal::create(&fixture.0.join("new.asb.zip"), manifest(0), 40, limits).is_err()
    );
    let mut recovered = RecordingJournal::recover(&destination, limits).unwrap();
    assert_eq!(recovered.snapshot().unwrap().tracks.len(), 1);
    assert!(recovered.finalize().unwrap().cleanup_complete);
}

#[test]
fn journal_retains_interrupted_copy_without_promoting_it_to_complete() {
    struct BrokenReader(bool);
    impl Read for BrokenReader {
        fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
            if self.0 {
                return Err(std::io::Error::other("injected copy failure"));
            }
            self.0 = true;
            bytes[..3].copy_from_slice(b"{}\n");
            Ok(3)
        }
    }
    let fixture = Fixture::new();
    let destination = fixture.0.join("trials.asb.zip");
    let mut journal =
        RecordingJournal::create(&destination, manifest(0), 1, Limits::default()).unwrap();
    journal.begin(1, 1).unwrap();
    assert!(
        journal
            .commit(manifest(1).tracks[0].clone(), &mut BrokenReader(false))
            .is_err()
    );
    fs::write(
        journal.partial_directory().join(".pending-torn-diagnostic"),
        "{torn",
    )
    .unwrap();
    let expected = journal.snapshot().unwrap();
    assert_eq!(expected.tracks[0].bytes, 3);
    assert!(!expected.tracks[0].complete);
    assert_eq!(expected.tracks[0].workload_success, None);
    drop(journal);
    let mut recovered = RecordingJournal::recover(&destination, Limits::default()).unwrap();
    assert_eq!(recovered.snapshot().unwrap(), expected);
    assert!(
        recovered
            .commit(manifest(1).tracks[0].clone(), &mut &b"{}\n"[..])
            .is_err(),
        "must not overwrite interrupted payload"
    );
    assert_eq!(
        fs::read(
            recovered
                .partial_directory()
                .join(".pending-torn-diagnostic")
        )
        .unwrap(),
        b"{torn"
    );
}

#[test]
fn journal_refuses_existing_destinations_and_bounds_before_starting_slots() {
    let fixture = Fixture::new();
    let destination = fixture.0.join("trials.asb.zip");
    let limits = Limits {
        max_tracks: 2,
        max_track_bytes: 3,
        max_total_bytes: 3,
        ..Limits::default()
    };
    assert!(RecordingJournal::create(&destination, manifest(0), 3, limits).is_err());
    assert!(RecordingJournal::create(&destination, manifest(0), 0, limits).is_err());
    assert!(RecordingJournal::create(&destination, manifest(0), u64::MAX, limits).is_err());
    assert!(!fixture.0.join(".trials.asb.zip.partial").exists());
    fs::write(&destination, b"user data").unwrap();
    assert!(RecordingJournal::create(&destination, manifest(0), 1, limits).is_err());
    assert_eq!(fs::read(&destination).unwrap(), b"user data");
    let destination = fixture.0.join("new.zip");
    let mut journal = RecordingJournal::create(&destination, manifest(0), 2, limits).unwrap();
    assert!(journal.begin(0, 1).is_err());
    assert!(journal.begin(1, 2).is_err());
    assert!(journal.begin(3, 1).is_err());
    journal.begin(1, 1).unwrap();
    journal
        .commit(manifest(1).tracks[0].clone(), &mut &b"{}\n"[..])
        .unwrap();
    journal.begin(2, 1).unwrap();
    assert!(
        journal
            .commit(manifest(2).tracks[1].clone(), &mut &b"{}\n"[..])
            .is_err()
    );
    assert!(!journal.partial_directory().join("track-2.jsonl").exists());
    drop(journal);
    assert!(RecordingJournal::create(&destination, manifest(0), 2, limits).is_err());
    let recovered = RecordingJournal::recover(&destination, limits).unwrap();
    assert_eq!(recovered.snapshot().unwrap().tracks.len(), 2);
}

#[test]
fn journal_recovery_rejects_payload_and_committed_identity_tampering() {
    for variant in 0..4 {
        let fixture = Fixture::new();
        let destination = fixture.0.join("trials.asb.zip");
        let mut journal =
            RecordingJournal::create(&destination, manifest(0), 1, Limits::default()).unwrap();
        journal.begin(1, 1).unwrap();
        journal
            .commit(manifest(1).tracks[0].clone(), &mut &b"{}\n"[..])
            .unwrap();
        let root = journal.partial_directory().to_path_buf();
        drop(journal);
        match variant {
            0 => fs::write(root.join("track-1.jsonl"), b"[]\n").unwrap(),
            1 => fs::remove_file(root.join("started-1.json")).unwrap(),
            2 => fs::write(root.join("committed-1.json"), b"{torn").unwrap(),
            _ => {
                let path = root.join("committed-1.json");
                let mut record: serde_json::Value =
                    serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
                record["recording_id"] = "another-recording".into();
                fs::write(path, serde_json::to_vec(&record).unwrap()).unwrap();
            }
        }
        assert!(
            RecordingJournal::recover(&destination, Limits::default()).is_err(),
            "accepted variant {variant}"
        );
    }
}

#[cfg(unix)]
#[test]
fn journal_rejects_replaced_partial_directory_and_payload_symlinks() {
    use std::os::unix::fs::symlink;
    let fixture = Fixture::new();
    let destination = fixture.0.join("trials.asb.zip");
    let mut journal =
        RecordingJournal::create(&destination, manifest(0), 1, Limits::default()).unwrap();
    journal.begin(1, 1).unwrap();
    let root = journal.partial_directory().to_path_buf();
    let unrelated = fixture.0.join("user-data");
    fs::write(&unrelated, b"user data").unwrap();
    symlink(&unrelated, root.join("track-1.jsonl")).unwrap();
    assert!(journal.snapshot().is_err());
    assert!(
        journal
            .commit(manifest(1).tracks[0].clone(), &mut &b"{}\n"[..])
            .is_err()
    );
    assert_eq!(fs::read(&unrelated).unwrap(), b"user data");
    let preserved = fixture.0.join("preserved-partial");
    fs::rename(&root, &preserved).unwrap();
    fs::create_dir(&root).unwrap();
    assert!(journal.begin(1, 1).is_err());
    assert!(journal.snapshot().is_err());
    assert!(preserved.join("intent.json").is_file());
}

#[test]
fn journal_publication_fault_matrix_requires_recovery_without_losing_real_state() {
    let points = [
        "before-write",
        "before-file-sync",
        "after-file-sync",
        "before-link",
        "after-link",
        "before-directory-sync",
        "after-directory-sync",
        "before-temporary-removal",
        "after-temporary-removal",
    ];
    for commit in [false, true] {
        for point in points {
            let fixture = Fixture::new();
            let destination = fixture.0.join("fault.zip");
            let mut journal =
                RecordingJournal::create(&destination, manifest(0), 2, Limits::default()).unwrap();
            if commit {
                journal.begin(1, 1).unwrap();
            }
            journal.inject_publication_failure(point);
            let failure = if commit {
                journal
                    .commit(manifest(1).tracks[0].clone(), &mut &b"{}\n"[..])
                    .expect_err("fault must fire")
            } else {
                journal.begin(1, 1).expect_err("fault must fire")
            };
            assert!(
                failure.to_string().contains(point),
                "wrong failure: {failure}"
            );
            assert!(
                journal.snapshot().is_err(),
                "ambiguous publication must not downgrade/omit durable records"
            );
            assert!(
                journal.begin(2, 1).is_err(),
                "poisoned owner must not admit new work"
            );
            let root = journal.partial_directory().to_path_buf();
            let started = root.join("started-1.json").exists();
            let committed = root.join("committed-1.json").exists();
            drop(journal);
            let recovered = RecordingJournal::recover(&destination, Limits::default()).unwrap();
            let actual = recovered.snapshot().unwrap();
            assert_eq!(actual.tracks.len(), usize::from(started));
            if started {
                assert_eq!(actual.tracks[0].complete, committed);
                assert_eq!(actual.tracks[0].process_success, committed.then_some(true));
                assert_eq!(actual.tracks[0].bytes, if commit { 3 } else { 0 });
            }
        }
    }
}

#[cfg(unix)]
#[test]
fn journal_recovery_checks_aggregate_budgets_before_reading_payloads() {
    use std::os::unix::fs::symlink;
    let fixture = Fixture::new();
    let destination = fixture.0.join("budget.zip");
    let mut journal =
        RecordingJournal::create(&destination, manifest(0), 2, Limits::default()).unwrap();
    for index in 1..=2 {
        journal.begin(index, 1).unwrap();
        journal
            .commit(
                manifest(2).tracks[(index - 1) as usize].clone(),
                &mut &b"{}\n"[..],
            )
            .unwrap();
    }
    let root = journal.partial_directory().to_path_buf();
    let metadata_bytes: u64 = [
        "intent.json",
        "started-1.json",
        "started-2.json",
        "committed-1.json",
        "committed-2.json",
    ]
    .iter()
    .map(|name| fs::metadata(root.join(name)).unwrap().len())
    .sum();
    drop(journal);
    // If recovery attempted payload opens before checking declared totals it
    // would fail with a symlink/I/O error, not the required aggregate error.
    fs::remove_file(root.join("track-1.jsonl")).unwrap();
    symlink(fixture.0.join("nonexistent"), root.join("track-1.jsonl")).unwrap();
    let limits = Limits {
        max_total_bytes: 5,
        ..Limits::default()
    };
    let error = RecordingJournal::recover(&destination, limits)
        .err()
        .expect("aggregate must fail");
    assert!(
        error.to_string().contains("aggregate"),
        "payload read preceded aggregate check: {error}"
    );
    let limits = Limits {
        max_manifest_bytes: metadata_bytes - 1,
        ..Limits::default()
    };
    let error = RecordingJournal::recover(&destination, limits)
        .err()
        .expect("metadata aggregate must fail");
    assert!(
        error.to_string().contains("bounded"),
        "metadata bound was not enforced before payload read: {error}"
    );
}

#[test]
fn journal_recovery_recounts_native_sidecars_before_the_next_commit() {
    let fixture = Fixture::new();
    let destination = fixture.0.join("native-budget.zip");
    let native_bytes = b"{\"event\":\"tool\"}\n";
    let native = NativeTrack {
        complete: true,
        events: 1,
        bytes: native_bytes.len() as u64,
        sha256: digest(native_bytes),
    };
    let limits = Limits {
        max_total_bytes: 2 * (3 + native.bytes),
        ..Limits::default()
    };
    let mut journal = RecordingJournal::create(&destination, manifest(0), 2, limits).unwrap();
    journal.begin(1, 1).unwrap();
    let mut provider = b"{}\n".as_slice();
    let mut source = native_bytes.as_slice();
    journal
        .commit_with_native(
            manifest(2).tracks[0].clone(),
            &mut provider,
            Vec::new(),
            Some(NativeInput {
                native: native.clone(),
                source: &mut source,
            }),
        )
        .unwrap();
    drop(journal);

    let mut recovered = RecordingJournal::recover(&destination, limits).unwrap();
    assert_eq!(recovered.staged_bytes(), 3 + native.bytes);
    recovered.begin(2, 1).unwrap();
    let mut provider = b"{}\n".as_slice();
    let mut source = native_bytes.as_slice();
    recovered
        .commit_with_native(
            manifest(2).tracks[1].clone(),
            &mut provider,
            Vec::new(),
            Some(NativeInput {
                native,
                source: &mut source,
            }),
        )
        .unwrap();
    recovered.finalize().unwrap();
}

#[test]
fn private_sidecar_cleanup_warnings_use_catalogue_labels_not_staging_paths() {
    for (skip, expected) in [(1, "auxiliary artifact local-tools"), (2, "native sidecar")] {
        let fixture = Fixture::new();
        let destination = fixture.0.join("recording.zip");
        let mut journal =
            RecordingJournal::create(&destination, manifest(0), 1, Limits::default()).unwrap();
        journal.begin(1, 1).unwrap();
        let artifact_bytes = b"{\"artifact\":true}\n";
        let native_bytes = b"{\"event\":\"tool\"}\n";
        let artifact = Artifact {
            kind: "local-tools".into(),
            bytes: artifact_bytes.len() as u64,
            sha256: digest(artifact_bytes),
        };
        let native = NativeTrack {
            complete: true,
            events: 1,
            bytes: native_bytes.len() as u64,
            sha256: digest(native_bytes),
        };
        let mut provider = b"{}\n".as_slice();
        let mut artifact_source = artifact_bytes.as_slice();
        let mut native_source = native_bytes.as_slice();
        journal
            .commit_with_native(
                manifest(1).tracks.remove(0),
                &mut provider,
                vec![ArtifactInput {
                    artifact,
                    source: &mut artifact_source,
                }],
                Some(NativeInput {
                    native,
                    source: &mut native_source,
                }),
            )
            .unwrap();
        let private_root = journal.partial_directory().display().to_string();
        journal.inject_publication_failure_after("after-payload-cleanup", skip);
        let publication = journal.finalize().unwrap();
        assert!(!publication.cleanup_complete);
        assert!(
            publication
                .warnings
                .iter()
                .any(|warning| warning.contains(expected))
        );
        assert!(
            publication
                .warnings
                .iter()
                .all(|warning| !warning.contains(&private_root)
                    && !warning.contains("native-1.jsonl"))
        );
    }
}

#[test]
fn schema_v1_archive_without_native_sidecars_remains_readable() {
    let fixture = Fixture::new();
    let mut legacy = manifest(1);
    legacy.schema_version = 1;
    let path = fixture.zip(&legacy, &[("tracks/1.jsonl", b"{}\n".as_slice())]);
    let mut archive = Archive::open(&path, Limits::default()).unwrap();
    assert_eq!(archive.manifest(), &legacy);
    assert!(archive.manifest().tracks[0].native.is_none());
    assert_eq!(archive.read_track(1).unwrap(), b"{}\n");
}
