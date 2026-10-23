use super::*;
use serde_json::Value;

#[test]
fn filesystem_and_verification_classes_are_specific_without_exposing_values() {
    for (text, expected) in [
        ("Permission denied (os error 13)", "permission denied"),
        ("No space left on device", "storage is full"),
        ("Read-only file system", "storage is read-only"),
        ("Not a directory", "a required parent is not a directory"),
        ("Is a directory", "a required file path is a directory"),
        (
            "No such file or directory",
            "a required file or directory is missing",
        ),
        ("File exists", "the evidence destination already exists"),
        (
            "checkpoint hashing needs a usable local Python 3 interpreter",
            "a usable local Python 3 interpreter is required for evidence verification",
        ),
        (
            "task completion directory must not be a symlink or non-directory",
            "the evidence directory is a symlink or non-directory",
        ),
        (
            "checkpoint hashing exceeded its deadline",
            "evidence verification exceeded its deadline",
        ),
        (
            "existing input changed",
            "existing evidence changed or does not match",
        ),
        (
            "completed task cannot be reused",
            "existing evidence did not pass continuation validation",
        ),
        (
            "checkpoint inputs could not be sealed",
            "declared inputs are missing, changing, unsupported or excessive",
        ),
        (
            "runtime inputs discovery failed safely with code `resource_bound`",
            "declared runtime inputs exceed the supported verification bound",
        ),
        (
            "runtime inputs discovery failed safely with code `required_runtime_unavailable`",
            "a declared runtime executable is unavailable or not executable",
        ),
        (
            "runtime inputs discovery failed safely with code `runtime_entry_unsupported`",
            "a declared runtime entry has an unsupported executable or interpreter format",
        ),
        (
            "runtime inputs discovery failed safely with code `runtime_input_validation_failed`",
            "a declared runtime input could not be safely verified",
        ),
    ] {
        assert_eq!(safe_reason(&Error::Io(text.into())), expected);
    }
    assert_eq!(
        safe_reason(&Error::Run(
            "private helper output: must-stay-private".into()
        )),
        "evidence verification or publication failed"
    );
    assert_eq!(
        safe_reason(&Error::Input("must-stay-private".into())),
        "evidence validation failed"
    );
}

#[test]
fn typed_json_events_preserve_paths_and_hide_arbitrary_error_contents() {
    let warning = Warning::new(
        Stage::Outcome,
        Path::new("scope"),
        Path::new("directory/é\nfile"),
        &Error::Io("super-secret-fingerprint-key=never-print-me".into()),
    );
    let seen = Mutex::new(BTreeSet::new());
    let mut output = Vec::new();
    warning.emit(true, &mut output, &seen).unwrap();
    let text = String::from_utf8(output).unwrap();
    assert_eq!(text.lines().count(), 1);
    assert!(!text.contains("never-print-me"));
    let value: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(value["event"], "continuation_warning");
    assert_eq!(value["schema_version"], 1);
    assert_eq!(value["code"], "task_outcome_unavailable");
    assert_eq!(value["path"], "directory/é\nfile");
    assert_eq!(value["path_role"], "destination");
    assert!(value.get("task").is_none());
    assert!(value.get("workload_success").is_none());
    assert!(
        value["remediation"]
            .as_str()
            .unwrap()
            .contains("Do not rerun")
    );
    let mut human = Vec::new();
    warning
        .emit(false, &mut human, &Mutex::new(BTreeSet::new()))
        .unwrap();
    let human = String::from_utf8(human).unwrap();
    assert!(!human.contains("é\nfile"));
    assert!(human.contains("é\\nfile"));
    assert!(!human.contains("never-print-me"));
}

#[test]
fn unknown_observer_names_its_source_outcome_not_a_failed_destination() {
    let warning = Warning::new(
        Stage::Replica,
        Path::new("scope"),
        Path::new("existing-outcome.json"),
        &Error::Io("fixture".into()),
    )
    .source_outcome();
    let mut output = Vec::new();
    warning
        .emit(true, &mut output, &Mutex::new(BTreeSet::new()))
        .unwrap();
    let value: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(value["path_role"], "source_outcome");
    assert_eq!(value["path"], "existing-outcome.json");
    let mut human = Vec::new();
    warning
        .emit(false, &mut human, &Mutex::new(BTreeSet::new()))
        .unwrap();
    let human = String::from_utf8(human).unwrap();
    assert!(
        human.contains("source outcome at existing-outcome.json (receipt destination unknown)")
    );
    assert!(!human.contains("unavailable at existing-outcome"));
}

#[test]
fn concurrent_identical_diagnostics_deduplicate_but_distinct_causes_and_scopes_survive() {
    let seen = Mutex::new(BTreeSet::new());
    let outputs = std::thread::scope(|scope| {
        let jobs = (0..8)
            .map(|_| {
                scope.spawn(|| {
                    let mut output = Vec::new();
                    Warning::new(
                        Stage::Replica,
                        Path::new("same-config"),
                        Path::new("record"),
                        &Error::Io("Permission denied".into()),
                    )
                    .emit(true, &mut output, &seen)
                    .unwrap();
                    output
                })
            })
            .collect::<Vec<_>>();
        jobs.into_iter()
            .flat_map(|job| job.join().unwrap())
            .collect::<Vec<_>>()
    });
    assert_eq!(String::from_utf8(outputs).unwrap().lines().count(), 1);
    for (scope, cause) in [
        ("same-config", "No space left"),
        ("other-config", "Permission denied"),
        ("same-config", "different-private-cause"),
    ] {
        let mut output = Vec::new();
        Warning::new(
            Stage::Replica,
            Path::new(scope),
            Path::new("record"),
            &Error::Io(cause.into()),
        )
        .emit(true, &mut output, &seen)
        .unwrap();
        assert!(!output.is_empty());
    }
    assert_eq!(seen.lock().unwrap().len(), 4);
}

#[test]
fn a_failed_writer_does_not_mark_delivery_or_erase_the_primary_failure() {
    struct Broken;
    impl Write for Broken {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "fixture writer closed",
            ))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let seen = Mutex::new(BTreeSet::new());
    let warning = Warning::new(
        Stage::Group,
        Path::new("scope"),
        Path::new("receipt"),
        &Error::Io("Permission denied".into()),
    );
    let output = warning.emit(true, &mut Broken, &seen);
    assert!(seen.lock().unwrap().is_empty());
    let error = preserve_primary(
        Err(Error::Run("original process or grader failure".into())),
        output,
    )
    .unwrap_err();
    assert!(
        matches!(error, Error::Io(_)),
        "presentation failure must stop admission"
    );
    let error = error.to_string();
    assert!(error.contains("original process or grader failure"));
    assert!(error.contains("fixture writer closed"));
    let mut output = Vec::new();
    warning.emit(true, &mut output, &seen).unwrap();
    assert!(!output.is_empty());
}
