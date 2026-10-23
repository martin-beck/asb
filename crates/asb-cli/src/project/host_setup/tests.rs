use super::*;
use std::io::{self, Cursor};

#[derive(Default)]
struct Fake {
    available: bool,
    probes: usize,
    calls: Vec<(SudoMode, Operation)>,
    failures: Vec<usize>,
}
impl Executor for Fake {
    fn passwordless(&mut self) -> bool {
        self.probes += 1;
        self.available
    }
    fn execute(&mut self, mode: SudoMode, operation: &Operation) -> bool {
        self.calls.push((mode, operation.clone()));
        !self.failures.contains(&self.calls.len())
    }
}
fn request(requirement: Requirement, failure: FailurePolicy) -> Request {
    Request {
        heading: "group commands",
        explanation: "separate approval required",
        operations: vec![
            Operation::command(&["mkdir", "-p", "/fixture/with space"], false),
            Operation::command(&["chmod", "0750", "/fixture/with space"], false),
            Operation::write("/fixture/control", b"+cpu\n"),
        ],
        requirement,
        failure,
    }
}

#[test]
fn explicit_consent_matrix_never_probes_or_executes_declined_commands() {
    for (answer, approved, prompts) in [
        ("y\n", true, 1),
        ("Y\n", true, 1),
        ("n\n", false, 1),
        ("N\n", false, 1),
        ("\n", false, 1),
        ("yes\nn\n", false, 2),
        (" y\nN\n", false, 2),
        ("", false, 1),
    ] {
        let mut fake = Fake {
            available: true,
            ..Default::default()
        };
        let mut reader = Cursor::new(answer);
        let mut output = Vec::new();
        let mut policy = Policy::new(&mut reader, &mut fake);
        match policy
            .approve(
                &mut output,
                false,
                request(Requirement::Passwordless, FailurePolicy::Stop),
            )
            .unwrap()
        {
            Decision::Approved(group) => {
                assert!(approved);
                assert_eq!(policy.execute(group), [true, true, true]);
            }
            Decision::Declined => assert!(!approved),
            Decision::Unavailable => panic!("fake capability is available"),
        }
        assert_eq!(fake.probes, usize::from(approved));
        assert_eq!(fake.calls.len(), if approved { 3 } else { 0 });
        let text = String::from_utf8(output).unwrap();
        assert!(text.contains("sudo -n mkdir -p '/fixture/with space'"));
        assert!(text.contains("sudo -n tee /fixture/control"));
        assert!(text.find("chmod").unwrap() < text.find("Apply the listed").unwrap());
        assert_eq!(
            text.matches("Apply the listed privileged commands? y/N:")
                .count(),
            prompts
        );
    }
}

#[test]
fn capability_is_cached_but_never_shared_consent_or_interactive_escalation() {
    for available in [false, true] {
        let mut fake = Fake {
            available,
            ..Default::default()
        };
        let mut reader = Cursor::new("y\nn\ny\n");
        let mut output = Vec::new();
        let mut policy = Policy::new(&mut reader, &mut fake);
        let Decision::Approved(first) = policy
            .approve(
                &mut output,
                false,
                request(Requirement::AllowInteractive, FailurePolicy::Continue),
            )
            .unwrap()
        else {
            panic!()
        };
        policy.execute(first);
        assert!(matches!(
            policy
                .approve(
                    &mut output,
                    false,
                    request(Requirement::Passwordless, FailurePolicy::Stop)
                )
                .unwrap(),
            Decision::Declined
        ));
        let last = policy
            .approve(
                &mut output,
                false,
                request(Requirement::Passwordless, FailurePolicy::Stop),
            )
            .unwrap();
        match last {
            Decision::Approved(group) => {
                assert!(available);
                policy.execute(group);
            }
            Decision::Unavailable => assert!(!available),
            Decision::Declined => panic!("third answer approved"),
        }
        assert_eq!(fake.probes, 1);
        assert_eq!(fake.calls.len(), if available { 6 } else { 3 });
        let expected = if available {
            SudoMode::Passwordless
        } else {
            SudoMode::Interactive
        };
        assert!(fake.calls.iter().all(|(mode, _)| *mode == expected));
        assert_eq!(
            String::from_utf8(output)
                .unwrap()
                .matches("Apply the listed")
                .count(),
            3
        );
    }
}

#[test]
fn failure_policy_preserves_independent_settings_and_dependent_cgroup_order() {
    for failure in [FailurePolicy::Continue, FailurePolicy::Stop] {
        for fail_at in 1..=3 {
            let mut fake = Fake {
                available: true,
                failures: vec![fail_at],
                ..Default::default()
            };
            let mut reader = Cursor::new("");
            let mut policy = Policy::new(&mut reader, &mut fake);
            let Decision::Approved(group) = policy
                .approve(
                    &mut Vec::new(),
                    true,
                    request(Requirement::Passwordless, failure),
                )
                .unwrap()
            else {
                panic!()
            };
            let results = policy.execute(group);
            let expected = if matches!(failure, FailurePolicy::Stop) {
                fail_at
            } else {
                3
            };
            assert_eq!(results.len(), expected);
            assert!(!results[fail_at - 1]);
            assert_eq!(fake.calls[0].1.args[0], "mkdir");
            if expected == 3 {
                assert_eq!(fake.calls[2].1.input.as_deref(), Some(b"+cpu\n".as_slice()));
            }
        }
    }
}

struct BrokenWriter {
    flush_only: bool,
}
impl Write for BrokenWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.flush_only {
            Ok(bytes.len())
        } else {
            Err(io::Error::other("fixture output failure"))
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        Err(io::Error::other("fixture flush failure"))
    }
}
struct BrokenReader;
impl io::Read for BrokenReader {
    fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
        Err(io::Error::other("fixture input failure"))
    }
}
impl io::BufRead for BrokenReader {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        Err(io::Error::other("fixture input failure"))
    }
    fn consume(&mut self, _: usize) {}
}

#[test]
fn prompt_reader_writer_and_flush_errors_cannot_authorize_execution() {
    for yes in [false, true] {
        let mut fake = Fake::default();
        let mut reader = Cursor::new("y\n");
        let mut policy = Policy::new(&mut reader, &mut fake);
        let error = policy
            .approve(
                &mut BrokenWriter { flush_only: false },
                yes,
                request(Requirement::AllowInteractive, FailurePolicy::Continue),
            )
            .err()
            .unwrap();
        assert!(error.to_string().contains("fixture output failure"));
        assert_eq!(fake.probes, 0);
        assert!(fake.calls.is_empty());
    }
    let mut fake = Fake::default();
    let mut reader = Cursor::new("y\n");
    let mut policy = Policy::new(&mut reader, &mut fake);
    assert!(
        policy
            .approve(
                &mut BrokenWriter { flush_only: true },
                false,
                request(Requirement::AllowInteractive, FailurePolicy::Continue)
            )
            .is_err()
    );
    assert_eq!(fake.probes, 0);
    let mut reader = BrokenReader;
    let mut policy = Policy::new(&mut reader, &mut fake);
    let error = policy
        .approve(
            &mut Vec::new(),
            false,
            request(Requirement::AllowInteractive, FailurePolicy::Continue),
        )
        .err()
        .unwrap();
    assert!(
        error
            .to_string()
            .contains("cannot read host capability confirmation: fixture input failure")
    );
    assert_eq!(fake.probes, 0);
    assert!(fake.calls.is_empty());
}

#[test]
fn real_settings_orchestration_preserves_best_effort_and_one_probe() {
    let mut fake = Fake {
        failures: vec![1],
        ..Default::default()
    };
    let mut reader = Cursor::new("");
    let mut output = Vec::new();
    let mut policy = Policy::new(&mut reader, &mut fake);
    assert!(
        super::super::configure_host_capabilities_with(&mut output, true, &mut policy, |_| None)
            .unwrap()
            .is_none()
    );
    assert_eq!(fake.probes, 1);
    assert_eq!(fake.calls.len(), 2);
    assert_eq!(
        fake.calls[0].1.args,
        ["sysctl", "-w", "kernel.perf_event_paranoid=-1"]
    );
    assert_eq!(
        fake.calls[1].1.args,
        ["sysctl", "-w", "kernel.sched_schedstats=1"]
    );
    assert!(
        fake.calls
            .iter()
            .all(|(mode, _)| *mode == SudoMode::Interactive)
    );
    let text = String::from_utf8(output).unwrap();
    assert!(text.contains("could not change kernel.perf_event_paranoid=-1"));
    assert!(text.contains("changed kernel.sched_schedstats=1"));
}

#[test]
fn existing_settings_need_no_consent_and_unavailable_sudo_never_mutates() {
    let mut fake = Fake::default();
    let mut reader = BrokenReader;
    let mut output = Vec::new();
    let mut policy = Policy::new(&mut reader, &mut fake);
    assert!(
        super::super::configure_host_capabilities_with(
            &mut output,
            false,
            &mut policy,
            |setting| {
                Some(
                    if setting.ends_with("paranoid") {
                        "-1\n"
                    } else {
                        "1\n"
                    }
                    .into(),
                )
            }
        )
        .unwrap()
        .is_none()
    );
    assert_eq!(fake.probes, 1);
    assert!(fake.calls.is_empty());
    assert_eq!(
        String::from_utf8(output)
            .unwrap()
            .matches("already enabled")
            .count(),
        2
    );
}

#[test]
fn partial_ready_settings_execute_only_the_pending_setting() {
    let mut fake = Fake::default();
    let mut reader = Cursor::new("y\n");
    let mut output = Vec::new();
    let mut policy = Policy::new(&mut reader, &mut fake);
    let result = super::super::configure_host_capabilities_with(
        &mut output,
        false,
        &mut policy,
        |setting| {
            Some(
                if setting.ends_with("paranoid") {
                    "-1\n"
                } else {
                    "0\n"
                }
                .into(),
            )
        },
    )
    .unwrap();
    assert!(result.is_none());
    assert_eq!(fake.probes, 1);
    assert_eq!(fake.calls.len(), 1);
    assert_eq!(
        fake.calls[0].1.args,
        ["sysctl", "-w", "kernel.sched_schedstats=1"]
    );
    let text = String::from_utf8(output).unwrap();
    assert_eq!(
        text.matches("kernel.perf_event_paranoid=-1 already enabled")
            .count(),
        1
    );
    assert_eq!(text.matches("changed kernel.sched_schedstats=1").count(), 1);
    assert!(!text.contains("sudo sysctl -w kernel.perf_event_paranoid"));
    assert!(!text.contains("changed kernel.perf_event_paranoid"));
}

#[test]
fn final_preexecution_flush_failure_after_consent_and_probe_grants_no_permit() {
    struct FinalFlushFailure {
        output: Vec<u8>,
        flushes: usize,
    }
    impl Write for FinalFlushFailure {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.output.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            self.flushes += 1;
            if self.flushes == 2 {
                Err(io::Error::other("fixture final approval flush failed"))
            } else {
                Ok(())
            }
        }
    }
    let mut fake = Fake {
        available: true,
        ..Default::default()
    };
    let mut reader = Cursor::new("y\n");
    let mut output = FinalFlushFailure {
        output: Vec::new(),
        flushes: 0,
    };
    let mut policy = Policy::new(&mut reader, &mut fake);
    let error =
        super::super::configure_host_capabilities_with(&mut output, false, &mut policy, |_| None)
            .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("fixture final approval flush failed")
    );
    assert_eq!(
        output.flushes, 2,
        "consent flush succeeded; final approval flush failed"
    );
    assert_eq!(
        reader.position(),
        2,
        "affirmative consent was actually consumed"
    );
    assert_eq!(fake.probes, 1, "read-only capability probe actually ran");
    assert!(
        fake.calls.is_empty(),
        "no operation may execute without a published permit"
    );
    let text = String::from_utf8(output.output).unwrap();
    assert!(text.contains("Checking passwordless sudo"));
    assert!(text.contains("Applying the commands above with passwordless sudo"));
    assert!(!text.contains("Host capability setup: changed"));
}

#[test]
fn shell_display_quotes_metacharacters_without_changing_executor_arguments() {
    let operation = Operation::command(&["chown", "1:2", "/fixture/a'b;$(false)"], false);
    assert!(
        operation
            .display(Requirement::Passwordless)
            .contains("'/fixture/a'\\''b;$(false)'")
    );
    assert_eq!(operation.args[2], "/fixture/a'b;$(false)");
}

#[test]
fn actual_cgroup_group_has_distinct_consent_short_circuit_and_controller_failure() {
    for fail_at in 0..=4 {
        for enable_cpu in [false, true] {
            let mut fake = Fake {
                available: true,
                failures: vec![fail_at],
                ..Default::default()
            };
            let mut reader = Cursor::new("y\n");
            let mut policy = Policy::new(&mut reader, &mut fake);
            let mut output = Vec::new();
            let result = super::super::apply_cgroup_parent(
                &mut output,
                false,
                &mut policy,
                "/fixture/cgroup",
                "123:456",
                "/fixture/controller",
                enable_cpu,
            )
            .unwrap();
            let total = if enable_cpu { 4 } else { 3 };
            let failed = fail_at > 0 && fail_at <= total;
            assert_eq!(result.is_none(), failed);
            assert_eq!(fake.calls.len(), if failed { fail_at } else { total });
            assert_eq!(fake.probes, 1);
            assert!(
                fake.calls
                    .iter()
                    .all(|(mode, _)| *mode == SudoMode::Passwordless)
            );
            assert_eq!(fake.calls[0].1.args, ["mkdir", "-p", "/fixture/cgroup"]);
            let text = String::from_utf8(output).unwrap();
            if enable_cpu {
                assert!(text.contains("sudo -n tee /fixture/controller"));
            } else {
                assert!(!text.contains("tee /fixture/controller"));
            }
            if fail_at == 4 && enable_cpu {
                assert!(text.contains("could not enable the CPU controller"));
            }
        }
    }
    for (answer, available) in [("n\n", true), ("\n", true), ("y\n", false)] {
        let mut fake = Fake {
            available,
            ..Default::default()
        };
        let mut reader = Cursor::new(answer);
        let mut policy = Policy::new(&mut reader, &mut fake);
        assert!(
            super::super::apply_cgroup_parent(
                &mut Vec::new(),
                false,
                &mut policy,
                "/fixture/cgroup",
                "123:456",
                "/fixture/controller",
                true
            )
            .unwrap()
            .is_none()
        );
        assert!(fake.calls.is_empty());
    }
}
