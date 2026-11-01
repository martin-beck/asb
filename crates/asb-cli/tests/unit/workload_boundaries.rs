//! Offline filesystem and evaluator boundary checks. No agent or provider executes.
use super::adapters::prompt::evaluation::{evaluate_builtin, source_expectation};
use super::*;

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "asb-workload-boundaries-{}",
            TaskIdentity::local("boundary", None).attempt_id
        ));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
    fn write(&self, name: &str, bytes: impl AsRef<[u8]>) -> PathBuf {
        let path = self.0.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, bytes).unwrap();
        path
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn prompt_loading_distinguishes_invalid_missing_nonfile_empty_oversized_and_encoding() {
    let root = Fixture::new();
    assert!(
        load("original.", &root.0)
            .unwrap_err()
            .contains("invalid workload")
    );
    fs::create_dir(root.0.join("directory.txt")).unwrap();
    assert!(
        load("directory", &root.0)
            .unwrap_err()
            .contains("was not found")
    );
    root.write("blank.txt", " \n\t");
    assert!(load("blank", &root.0).unwrap_err().contains("is empty"));
    root.write("large.txt", vec![b'x'; MAX_PROMPT_BYTES as usize + 1]);
    assert!(
        load("large", &root.0)
            .unwrap_err()
            .contains("exceeds 64 KiB")
    );
    root.write("binary.txt", [0xff]);
    let error = load("binary", &root.0).unwrap_err();
    assert!(error.contains("cannot read workload `binary`"), "{error}");
    assert!(error.contains(&root.0.display().to_string()));
    root.write("valid.txt", "preserve trailing whitespace \n");
    assert_eq!(
        load("original.valid", &root.0).unwrap(),
        "preserve trailing whitespace \n"
    );
}

#[test]
fn selected_trial_falls_back_to_description_but_validates_set_and_override_bytes() {
    let root = Fixture::new();
    root.write("custom.txt", "base prompt");
    root.write(
        "custom.workload.toml",
        "workload='custom'\n[[trials]]\nid='one'\ndescription='A specific case.'\n",
    );
    let error = selected_trial("custom", "one", None, &root.0).unwrap_err();
    assert!(error.contains("without a trial-set TOML"));
    let prompt = load_trial("custom", Some("one"), Some("custom.workload.toml"), &root.0).unwrap();
    assert_eq!(
        prompt,
        "base prompt\n\nSelected external workload trial: one\nA specific case.\n"
    );
    let path = root.write("custom/trials/one.txt", "chosen prompt");
    assert_eq!(
        load_trial("custom", Some("one"), Some("custom.workload.toml"), &root.0).unwrap(),
        "chosen prompt"
    );
    for (bytes, expected) in [
        (Vec::new(), "is empty"),
        (vec![0xff], "cannot read trial `custom`"),
        (vec![b'x'; MAX_PROMPT_BYTES as usize + 1], "exceeds 64 KiB"),
    ] {
        fs::write(&path, bytes).unwrap();
        let error = load_prompt_file(&path, "custom").unwrap_err();
        assert!(error.contains(expected), "{error}");
        assert!(error.contains(&path.display().to_string()));
    }
    fs::remove_file(&path).unwrap();
    assert!(
        load_prompt_file(&path, "custom")
            .unwrap_err()
            .contains("cannot inspect trial")
    );
    root.write("empty.workload.toml", "workload='custom'\ntrials=[]\n");
    assert!(
        read_trial_set(&root.0.join("empty.workload.toml"))
            .unwrap_err()
            .contains("at least one trial")
    );
    assert!(
        read_trial_set(&root.0.join("missing.toml"))
            .unwrap_err()
            .contains("cannot read trial set")
    );
}

#[test]
fn bundled_graders_cover_build_and_dependency_solutions_without_running_them() {
    let root = Fixture::new();
    for name in ["build-repair", "dependency-migration"] {
        let expectation = source_expectation(name).unwrap();
        for file in expectation.files {
            root.write(file.relative, file.accepted[0]);
        }
        assert_eq!(
            evaluate_builtin(name, &root.0, "").unwrap(),
            Evaluation::Passed
        );
    }
    assert!(source_expectation("not-registered").is_none());
    assert!(
        evaluate_builtin("not-registered", &root.0, "")
            .unwrap_err()
            .contains("no bundled grader")
    );
    assert_eq!(
        prepare_builtin_fixture("not-registered", &root.0).unwrap(),
        0
    );
    assert_eq!(
        prepare_external("not-registered", &root.0, &root.0).unwrap(),
        0
    );
}

#[test]
fn external_grader_reports_directory_size_encoding_and_missing_outputs_separately() {
    let root = Fixture::new();
    let path = root.0.join("external-workload/asb-trial/src/task.py");
    fs::create_dir_all(&path).unwrap();
    assert!(
        matches!(evaluate_external_smoke("agentbench", &root.0, "").unwrap(), Evaluation::Failed(message) if message.contains("is not a file"))
    );
    fs::remove_dir(&path).unwrap();
    fs::write(&path, vec![b'x'; MAX_PROMPT_BYTES as usize + 1]).unwrap();
    assert!(
        matches!(evaluate_external_smoke("agentbench", &root.0, "").unwrap(), Evaluation::Failed(message) if message.contains("grading limit"))
    );
    fs::write(&path, [0xff]).unwrap();
    assert!(
        evaluate_external_smoke("agentbench", &root.0, "")
            .unwrap_err()
            .contains("cannot read adapter trial implementation")
    );
    fs::remove_file(&path).unwrap();
    assert!(
        matches!(evaluate_external_smoke("agentbench", &root.0, "").unwrap(), Evaluation::Failed(message) if message.contains("is missing"))
    );
}

#[test]
fn external_copy_handles_trial_only_source_preserves_edits_and_reports_blocked_destinations() {
    let root = Fixture::new();
    root.write("catalog/agentbench/asb-trial/src/task.py", "fixture");
    let catalog = root.0.join("catalog");
    let workspace = root.0.join("workspace");
    assert_eq!(
        prepare_external_source("agentbench", &catalog, &workspace).unwrap(),
        1
    );
    fs::write(
        workspace.join("external-workload/asb-trial/src/task.py"),
        "agent edit",
    )
    .unwrap();
    assert_eq!(
        prepare_external_source("agentbench", &catalog, &workspace).unwrap(),
        0
    );
    assert_eq!(
        fs::read_to_string(workspace.join("external-workload/asb-trial/src/task.py")).unwrap(),
        "agent edit"
    );
    let blocker = root.write("blocked", "not a directory");
    let error = copy_tree(&catalog, &blocker).unwrap_err();
    assert!(error.contains("cannot create external workload directory"));
    let error = copy_plan(&root.0.join("absent")).unwrap_err();
    assert!(error.contains("cannot read external workload source"));
    let source = root.0.join("source");
    fs::create_dir_all(source.join("nested")).unwrap();
    let destination = root.0.join("destination");
    fs::create_dir(&destination).unwrap();
    fs::write(destination.join("nested"), "blocker").unwrap();
    assert!(
        copy_tree(&source, &destination)
            .unwrap_err()
            .contains("cannot create external workload directory")
    );
    fs::remove_dir(source.join("nested")).unwrap();
    fs::write(source.join("nested"), "file").unwrap();
    // Missing parent in destination yields a concrete copy error, not silent success.
    fs::create_dir(source.join("child")).unwrap();
    fs::write(source.join("child/file"), "x").unwrap();
    fs::write(destination.join("child"), "blocker").unwrap();
    assert!(copy_tree(&source, &destination).is_err());
}

#[cfg(unix)]
#[test]
fn symlink_loops_are_inspection_failures_not_false_success_or_missing_outputs() {
    use std::os::unix::fs::symlink;
    let root = Fixture::new();
    fs::create_dir_all(root.0.join("src")).unwrap();
    symlink("parser.py", root.0.join("src/parser.py")).unwrap();
    assert!(
        evaluate_builtin("bug-fix", &root.0, "")
            .unwrap_err()
            .contains("cannot inspect output file `src/parser.py`")
    );
    fs::create_dir_all(root.0.join("external-workload/asb-trial/src")).unwrap();
    symlink(
        "task.py",
        root.0.join("external-workload/asb-trial/src/task.py"),
    )
    .unwrap();
    assert!(
        evaluate_external_smoke("agentbench", &root.0, "")
            .unwrap_err()
            .contains("cannot inspect adapter trial output")
    );
}

#[test]
fn generic_install_dispatch_does_not_install_unknown_or_bundled_workloads() {
    let root = Fixture::new();
    let context = InstallationContext {
        process: &crate::install::OperationContext::new(
            &root.0,
            &crate::project::ProjectConfig::default(),
        )
        .unwrap(),
        project_root: &root.0,
        tools_root: &root.0,
        workloads_root: &root.0,
    };
    let mut output = Vec::new();
    let mut updates = Vec::new();
    for name in ["not-registered", "bug-fix"] {
        assert!(
            !install_registered(name, &context, &mut output, &mut |percent, message| updates
                .push((percent, message.to_owned())))
            .unwrap()
        );
    }
    assert!(output.is_empty());
    assert!(updates.is_empty());
}
