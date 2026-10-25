//! Inert processes only: never invoke a real perf sampler or require privileges.
use super::*;
use std::fs::{self, File};
use std::os::unix::{fs::PermissionsExt, process::ExitStatusExt};
use std::path::PathBuf;
use std::time::{Duration, Instant};

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/perf-stop-fixtures")
            .join(crate::workloads::TaskIdentity::local("perf-stop", None).attempt_id);
        fs::create_dir_all(&root).unwrap();
        Self(root)
    }
    fn session(&self) -> measurements::Session {
        measurements::Session::start_with_collectors(&self.0, &[], None).unwrap()
    }
}
fn until(mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !predicate() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn actual_requested_sigint_preserves_real_exit_and_requires_complete_stat_data() {
    for (controlled, valid) in [(true, true), (true, false), (false, true)] {
        let fixture = Fixture::new();
        let stdout = fixture.0.join("stdout");
        let stderr = fixture.0.join("stderr");
        let counters = if valid {
            "1.0,42,,cycles,100,100.0,,\\n"
        } else {
            "partial-counter-prefix"
        };
        let ending = if controlled {
            "signal.pause()"
        } else {
            "done()"
        };
        let script = format!(
            "import signal,sys,os\ndef done(*_):\n sys.stderr.write('{counters}');sys.stderr.flush()\n signal.signal(signal.SIGINT,signal.SIG_DFL)\n os.kill(os.getpid(),signal.SIGINT)\nsignal.signal(signal.SIGINT,done)\nprint('READY',flush=True)\n{ending}\n"
        );
        let mut launch = crate::launch::ProcessLaunch::new("python3");
        launch.arg("-I").arg("-S").arg("-u").arg("-c").arg(script);
        let (mut command, route) = measurements::producer_command(&mut launch).unwrap();
        let mut producer = measurements::Producer::spawn_with_dependency(
            &mut command,
            route,
            measurements::ProducerPolicy {
                stream_bytes: 4096,
                graceful: Some(Duration::from_secs(1)),
            },
            [
                File::create(&stdout).unwrap(),
                File::create(&stderr).unwrap(),
            ],
            None,
        )
        .unwrap();
        until(|| fs::read(&stdout).is_ok_and(|data| data.starts_with(b"READY\n")));
        let completion = if controlled {
            producer.stop()
        } else {
            let mut observed = None;
            until(|| {
                observed = producer.poll().unwrap();
                observed.is_some()
            });
            observed.unwrap()
        };
        assert_eq!(completion.exit.unwrap().signal(), Some(libc::SIGINT));
        assert_eq!(completion.exit.unwrap().code(), None);
        assert!(completion.capture_complete);
        assert_eq!(completion.guardian_cleanup_confirmed, Some(true));
        assert_eq!(completion.interruption_requested, controlled);
        assert_eq!(completion.interruption_acknowledged, controlled);
        assert_eq!(
            ADAPTER.accepts_termination("perf-stat", &completion),
            controlled
        );
        let mut captures = [
            crate::evidence::Capture::open(&stdout).unwrap(),
            crate::evidence::Capture::open(&stderr).unwrap(),
        ];
        for capture in &mut captures {
            capture.freeze().unwrap();
        }
        let context = ProcessContext::from_completion(
            "perf-stat",
            &completion,
            ["", ""],
            [&captures[0], &captures[1]],
        );
        let result = process(&mut fixture.session(), &context);
        match (controlled, valid) {
            (true, true) => {
                let result = result.unwrap();
                assert_eq!(result.status, "completed");
                assert_eq!(result.metrics["perf.observed_cycles_count"], 42.0);
            }
            (true, false) => assert!(result.is_err()),
            (false, _) => {
                let result = result.unwrap();
                assert_eq!(result.status, "failed");
                assert!(result.metrics.is_empty());
            }
        }
        assert!(stdout.is_file() && stderr.is_file());
    }
}

#[test]
fn installed_offline_decoder_runs_once_with_closed_environment_and_retains_failed_captures() {
    for mode in ["valid", "nonzero", "timeout", "overflow"] {
        let fixture = Fixture::new();
        let tools = fixture.0.join("tools");
        let bin = tools.join("perf/bin");
        fs::create_dir_all(&bin).unwrap();
        let count = fixture.0.join("invocations");
        let artifact = fixture.0.join("perf.data");
        fs::write(&artifact, "inert version-specific decoder fixture").unwrap();
        let artifact = artifact.canonicalize().unwrap();
        let action = match mode {
            "nonzero" => "exit 2",
            "timeout" => "/bin/sleep 2",
            "overflow" => "printf '012345678901234567890123456789'",
            _ => ":",
        };
        let script = format!(
            "#!/bin/sh\n[ \"$#\" = 3 ] && [ \"$1\" = script ] && [ \"$2\" = -i ] && [ \"$3\" = {artifact:?} ] || exit 81\n[ \"${{DEBUGINFOD_URLS-unset}}\" = '' ] && [ \"$PERF_CONFIG_NOSYSTEM\" = 1 ] && [ \"$PERF_CONFIG_NOGLOBAL\" = 1 ] || exit 82\n[ -n \"${{HOME-}}\" ] && [ -z \"${{LD_PRELOAD-}}\" ] || exit 83\nprintf 'once\\n' >> {count:?}\n{action}\n"
        );
        let executable = bin.join("perf");
        crate::write_test_file(&fixture.0, &executable, script).unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        let mut session = fixture.session();
        session.tool_dir = Some(tools);
        let result = if matches!(mode, "valid" | "nonzero") {
            session.set_artifact(SharedArtifact::StackSamples, artifact.clone());
            let mut completion = measurements::ProducerCompletion::test_completed(
                std::process::ExitStatus::from_raw(libc::SIGINT),
            );
            completion.interruption_requested = true;
            completion.interruption_acknowledged = true;
            completion.guardian_cleanup_confirmed = Some(true);
            let mut captures = [
                crate::evidence::Capture::create(&fixture.0.join("record.stdout")).unwrap(),
                crate::evidence::Capture::create(&fixture.0.join("record.stderr")).unwrap(),
            ];
            for capture in &mut captures {
                capture.freeze().unwrap();
            }
            let context = ProcessContext::from_completion(
                "perf",
                &completion,
                ["", ""],
                [&captures[0], &captures[1]],
            );
            assert!(ADAPTER.accepts_termination("perf", &completion));
            process(&mut session, &context).map(|data| {
                assert_eq!(data.status, "completed");
                assert!(data.artifact.is_some());
                assert!(
                    data.metrics.is_empty(),
                    "empty record must not invent samples"
                );
            })
        } else {
            validate_record(
                &session,
                &artifact,
                if mode == "timeout" {
                    Duration::from_millis(30)
                } else {
                    Duration::from_secs(2)
                },
                if mode == "overflow" { 8 } else { 4096 },
            )
        };
        assert_eq!(fs::read_to_string(&count).unwrap(), "once\n", "{mode}");
        assert_eq!(result.is_ok(), mode == "valid", "{mode}: {result:?}");
        assert_eq!(
            session.dir.join("perf-validation.stdout").exists(),
            mode != "valid"
        );
        assert_eq!(
            session.dir.join("perf-validation.stderr").exists(),
            mode != "valid"
        );
        assert!(artifact.is_file());
    }
}

#[test]
fn exit130_requires_all_control_facts_and_other_signals_never_qualify() {
    let mut completion = measurements::ProducerCompletion::test_completed(
        std::process::ExitStatus::from_raw(130 << 8),
    );
    assert!(!ADAPTER.accepts_termination("perf-stat", &completion));
    completion.interruption_requested = true;
    completion.interruption_acknowledged = true;
    completion.guardian_cleanup_confirmed = Some(true);
    assert!(ADAPTER.accepts_termination("perf-stat", &completion));
    completion.capture_complete = false;
    assert!(!ADAPTER.accepts_termination("perf-stat", &completion));
    completion.capture_complete = true;
    completion.exit = Some(std::process::ExitStatus::from_raw(libc::SIGTERM));
    assert!(!ADAPTER.accepts_termination("perf-stat", &completion));
    assert!(ADAPTER.retain_raw_on_processing_failure());
}
