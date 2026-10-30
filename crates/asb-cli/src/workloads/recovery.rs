//! Durable attempt locations and adapter-owned, non-destructive recovery.
use super::session::DatasetIdentity;
use crate::{Error, io_error};
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

pub(crate) type AllocationObserver<'a> = dyn Fn(&AttemptReference) -> Result<(), String> + 'a;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AttemptReference {
    pub schema_version: u32,
    pub preparation_id: String,
    pub workload: String,
    pub task_id: String,
    pub dataset: DatasetIdentity,
    pub artifacts: PathBuf,
}

impl AttemptReference {
    pub(crate) fn matches_selection(&self, selection: &super::TaskSelection) -> bool {
        self.schema_version == 1
            && self.workload == selection.workload
            && self.task_id == selection.task_id
            && Some(&self.dataset) == selection.dataset.as_ref()
    }

    pub(crate) fn validate_location(&self, project_root: &Path) -> Result<(), Error> {
        let root = std::path::absolute(project_root).map_err(io_error)?;
        if self.schema_version != 1
            || self.preparation_id.is_empty()
            || self.preparation_id.len() > 128
            || !self
                .preparation_id
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() || byte == b'-')
            || self.artifacts != root.join(".asb/attempts").join(&self.preparation_id)
            || self.workload.is_empty()
            || self.workload.len() > 256
            || self.task_id.is_empty()
            || self.task_id.len() > 256
        {
            return Err(Error::Input(
                "attempt reference is outside its project or has an invalid identity".into(),
            ));
        }
        let attempts = root.join(".asb/attempts");
        if attempts.canonicalize().map_err(io_error)?
            != root.canonicalize().map_err(io_error)?.join(".asb/attempts")
        {
            return Err(Error::Input(
                "attempt storage resolves outside its project".into(),
            ));
        }
        if let Ok(metadata) = std::fs::symlink_metadata(&self.artifacts)
            && !metadata.is_dir()
        {
            return Err(Error::Input(
                "attempt artifacts must be a real directory".into(),
            ));
        }
        Ok(())
    }
}

pub(crate) struct RecoveryContext<'a> {
    pub project_root: &'a Path,
    pub project_config: &'a crate::project::ProjectConfig,
    pub reference: &'a AttemptReference,
}

pub(crate) struct RetainedOutcomeContext<'a> {
    pub recovery: RecoveryContext<'a>,
    pub summary: &'a serde_json::Value,
}

pub(crate) fn verify_retained(context: &RetainedOutcomeContext<'_>) -> Result<(), Error> {
    super::adapters::find(&context.recovery.reference.workload)
        .ok_or_else(|| Error::Input("retained workload adapter is unavailable".into()))?
        .verify_retained(context)
}

/// Publish before controller spawn. A partial write cannot launch a task.
pub(super) fn remember(workspace: &Path, reference: &AttemptReference) -> Result<PathBuf, Error> {
    let directory = workspace.join(".asb-attempts");
    if let Ok(metadata) = std::fs::symlink_metadata(&directory)
        && !metadata.is_dir()
    {
        return Err(Error::Input(
            "attempt references require a real directory".into(),
        ));
    }
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(&directory).map_err(io_error)?;
    let path = directory.join(format!("{}.json", reference.preparation_id));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&path).map_err(io_error)?;
    let bytes = serde_json::to_vec(reference)
        .map_err(|_| Error::Io("cannot encode attempt reference".into()))?;
    file.write_all(&bytes)
        .and_then(|()| file.sync_all())
        .map_err(io_error)?;
    std::fs::File::open(&directory)
        .and_then(|file| file.sync_all())
        .map_err(io_error)?;
    std::fs::File::open(workspace)
        .and_then(|file| file.sync_all())
        .map_err(io_error)?;
    Ok(path)
}

fn read_reference(path: &Path) -> Result<AttemptReference, Error> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path).map_err(|error| {
        Error::Input(format!(
            "cannot read attempt reference {}: {error}",
            path.display()
        ))
    })?;
    let metadata = file.metadata().map_err(io_error)?;
    if !metadata.is_file() || metadata.len() > 64 * 1024 {
        return Err(Error::Input(
            "attempt reference must be a bounded regular file".into(),
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        #[cfg(target_os = "linux")]
        let uid = std::fs::metadata("/proc/self").map_err(io_error)?.uid();
        #[cfg(not(target_os = "linux"))]
        let uid = metadata.uid();
        if metadata.uid() != uid || metadata.mode() & 0o077 != 0 {
            return Err(Error::Input(
                "attempt reference must be private to the current user".into(),
            ));
        }
    }
    let mut bytes = Vec::new();
    file.take(64 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(io_error)?;
    if bytes.len() > 64 * 1024 {
        return Err(Error::Input(
            "attempt reference exceeds its size limit".into(),
        ));
    }
    serde_json::from_slice(&bytes)
        .map_err(|_| Error::Input("attempt reference is malformed".into()))
}

fn owning_project(
    reference: &AttemptReference,
) -> Result<(PathBuf, crate::project::ProjectConfig), Error> {
    // External workspaces must not select another project's SDK installation.
    let owner = reference
        .artifacts
        .ancestors()
        .nth(3)
        .ok_or_else(|| Error::Input("attempt reference has no owning project".into()))?;
    let (root, project_config) = crate::project::load(owner)?;
    reference.validate_location(&root)?;
    Ok((root, project_config))
}

pub(crate) fn inspect_grade(path: &Path, json: bool, output: &mut dyn Write) -> Result<(), Error> {
    let reference = read_reference(path)?;
    let report = inspect_reference(&reference)?;
    if json {
        serde_json::to_writer(&mut *output, &report)
            .map_err(|error| Error::Io(error.to_string()))?;
        writeln!(output).map_err(io_error)
    } else {
        writeln!(output, "Retained official grade for {} task {:?}: {}. No agent or evaluator ran; execution timing, exit status, tokens and steps remain unknown. No sweep outcome was imported.",
            reference.workload, reference.task_id, if report["workload_success"] == true { "resolved" } else { "unresolved" }).map_err(io_error)
    }
}

/// Validate existing grading artifacts without executing or installing anything.
pub(crate) fn inspect_reference(reference: &AttemptReference) -> Result<serde_json::Value, Error> {
    let (root, project_config) = owning_project(reference)?;
    super::adapters::find(&reference.workload)
        .ok_or_else(|| Error::Input("the attempt's workload adapter is unavailable".into()))?
        .inspect_grade(&RecoveryContext {
            project_root: &root,
            project_config: &project_config,
            reference,
        })
}

pub(crate) fn run(path: &Path, json: bool, output: &mut dyn Write) -> Result<(), Error> {
    let reference = read_reference(path)?;
    let (root, project_config) = owning_project(&reference)?;
    let adapter = super::adapters::find(&reference.workload)
        .ok_or_else(|| Error::Input("the attempt's workload adapter is unavailable".into()))?;
    let report = adapter.recover(&RecoveryContext {
        project_root: &root,
        project_config: &project_config,
        reference: &reference,
    })?;
    if json {
        serde_json::to_writer(&mut *output, &report)
            .map_err(|error| Error::Io(error.to_string()))?;
        writeln!(output).map_err(io_error)
    } else {
        let stopped = report["resources"].as_array().map_or(0, |resources| {
            resources
                .iter()
                .filter(|resource| resource["status"] == "stopped-preserved")
                .count()
        });
        writeln!(output, "Stopped {stopped} owned task containers; partial files, volumes and build images were preserved. Recovery did not retry or grade the task; sweep resume still requires outcome reconciliation.").map_err(io_error)
    }
}

pub(crate) fn grade_retained(path: &Path, json: bool, output: &mut dyn Write) -> Result<(), Error> {
    let reference = read_reference(path)?;
    let (root, project_config) = owning_project(&reference)?;
    let report = super::adapters::find(&reference.workload)
        .ok_or_else(|| Error::Input("the attempt's workload adapter is unavailable".into()))?
        .grade_retained(&RecoveryContext {
            project_root: &root,
            project_config: &project_config,
            reference: &reference,
        })?;
    if json {
        serde_json::to_writer(&mut *output, &report)
            .map_err(|error| Error::Io(error.to_string()))?;
        writeln!(output).map_err(io_error)
    } else {
        writeln!(output, "Official retained task grade for {} {:?}: {}. No agent was rerun. Execution metrics remain unknown; use `asb sweep SWEEP.toml --import-grades` to import the grade into its original sweep report.", reference.workload, reference.task_id, if report["workload_success"] == true { "resolved" } else { "unresolved" }).map_err(io_error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fixture {
        root: PathBuf,
        reference: AttemptReference,
    }
    impl Fixture {
        fn create() -> Self {
            let root = std::env::temp_dir()
                .join(super::super::TaskIdentity::local("attempt-reference-test", None).attempt_id);
            std::fs::create_dir_all(root.join(".asb/attempts")).unwrap();
            std::fs::write(
                root.join(".asb/project.toml"),
                toml::to_string(&crate::project::ProjectConfig::default()).unwrap(),
            )
            .unwrap();
            let reference = AttemptReference {
                schema_version: 1,
                preparation_id: "a-b-c".into(),
                workload: "swe-bench-lite".into(),
                task_id: "issue-1".into(),
                dataset: DatasetIdentity {
                    source: "fixture/dataset".into(),
                    revision: "pinned".into(),
                    split: "test".into(),
                },
                artifacts: root.join(".asb/attempts/a-b-c"),
            };
            Self { root, reference }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn attempt_references_are_private_immutable_and_not_grades() {
        let fixture = Fixture::create();
        fixture.reference.validate_location(&fixture.root).unwrap();
        assert!(
            !fixture.reference.artifacts.exists(),
            "location is durable before any task starts"
        );
        let path = remember(&fixture.root.join("workspace"), &fixture.reference).unwrap();
        let original = std::fs::read(&path).unwrap();
        assert_eq!(read_reference(&path).unwrap(), fixture.reference);
        assert!(remember(&fixture.root.join("workspace"), &fixture.reference).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), original);
        let value: serde_json::Value = serde_json::from_slice(&original).unwrap();
        assert!(value.get("workload_success").is_none());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
            assert!(read_reference(&path).is_err());
        }
    }

    #[test]
    fn invalid_locations_cannot_target_other_attempts_or_projects() {
        let fixture = Fixture::create();
        for preparation in ["../outside", "", "not-an-id"] {
            let mut reference = fixture.reference.clone();
            reference.preparation_id = preparation.into();
            assert!(reference.validate_location(&fixture.root).is_err());
        }
        let mut reference = fixture.reference.clone();
        reference.artifacts = fixture.root.join("outside");
        assert!(reference.validate_location(&fixture.root).is_err());
        #[cfg(unix)]
        {
            std::fs::create_dir(fixture.root.join("outside")).unwrap();
            std::os::unix::fs::symlink(fixture.root.join("outside"), &fixture.reference.artifacts)
                .unwrap();
            assert!(fixture.reference.validate_location(&fixture.root).is_err());
            let path = fixture.root.join("reference-link");
            std::os::unix::fs::symlink("missing", &path).unwrap();
            assert!(read_reference(&path).is_err());
        }
    }

    #[test]
    fn allocation_failure_prevents_controller_start_and_preserves_reference() {
        let fixture = Fixture::create();
        let project_config = crate::project::ProjectConfig::default();
        let refused = |_: &AttemptReference| Err("fixture allocation publication failed".into());
        let workspace = fixture.root.join("workspace");
        let result = super::super::remote::launch(
            "missing-sdk",
            "must-not-start.py",
            None,
            &fixture.reference.workload,
            fixture.reference.dataset.clone(),
            &super::super::PreparationContext {
                verify_initial_material: false,
                budgets: Default::default(),
                on_allocation: Some(&refused),
                workload: &fixture.reference.workload,
                trial: Some(&fixture.reference.task_id),
                trial_set: None,
                workload_directory: &fixture.root.join("workloads"),
                workspace: &workspace,
                project_root: &fixture.root,
                project_config: &project_config,
            },
        );
        assert!(matches!(result, Err(error) if error == "fixture allocation publication failed"));
        let references = std::fs::read_dir(workspace.join(".asb-attempts"))
            .unwrap()
            .collect::<Vec<_>>();
        assert_eq!(references.len(), 1);
        assert_eq!(
            std::fs::read_dir(fixture.root.join(".asb/attempts"))
                .unwrap()
                .count(),
            0
        );
    }

    #[cfg(unix)]
    #[test]
    fn every_original_adapter_recovers_through_its_own_project_tool() {
        use std::os::unix::fs::symlink;
        let fixture = Fixture::create();
        let external = Fixture::create();
        let python = crate::tools::host_executable(Path::new("python3")).unwrap();
        let settings = crate::project::ProjectConfig {
            tools_dir: "custom-tools".into(),
            ..Default::default()
        };
        std::fs::write(
            fixture.root.join(".asb/project.toml"),
            toml::to_string(&settings).unwrap(),
        )
        .unwrap();
        for (workload, tool) in [
            ("swe-bench", "swebench"),
            ("swe-bench-lite", "swebench"),
            ("swe-bench-verified", "swebench"),
            ("terminal-bench-1.0", "terminal-bench"),
            ("terminal-bench-2.0", "harbor"),
            ("terminal-bench-2.1", "harbor"),
        ] {
            let directory = fixture.root.join("custom-tools").join(tool);
            std::fs::create_dir_all(directory.join("venv/bin")).unwrap();
            if !directory.join("venv/bin/python").exists() {
                symlink(&python, directory.join("venv/bin/python")).unwrap();
            }
            std::fs::write(directory.join("asb_resources.py"), "import json,sys\nassert sys.argv[1]=='--quiesce'\na='asb-'+'a'*32\nprint(json.dumps({'schema_version':1,'attempt':a,'scope':'observed-resource-quiescence-not-a-retry-receipt','resources':[{'kind':'containers','name':a,'status':'stopped-preserved'},{'kind':'images','name':'asb-workload:'+a,'status':'preserved','image_id':'sha256:'+'b'*64}]}))\n").unwrap();
            let mut reference = fixture.reference.clone();
            reference.workload = workload.into();
            let elsewhere = remember(&external.root.join(workload), &reference).unwrap();
            run(&elsewhere, true, &mut Vec::new()).unwrap();
            let path = remember(&fixture.root.join(workload), &reference).unwrap();
            let mut json = Vec::new();
            run(&path, true, &mut json).unwrap();
            let record: serde_json::Value = serde_json::from_slice(&json).unwrap();
            assert_eq!(record["resources"][0]["status"], "stopped-preserved");
            assert_eq!(record["resources"][1]["kind"], "images");
            assert_eq!(
                record["resources"][1]["image_id"],
                format!("sha256:{}", "b".repeat(64))
            );
            let mut human = Vec::new();
            run(&path, false, &mut human).unwrap();
            assert!(
                String::from_utf8(human)
                    .unwrap()
                    .contains("did not retry or grade")
            );
            let helper = std::fs::read_to_string(directory.join("asb_resources.py")).unwrap();
            for invalid in ["None", "'sha256:invalid'"] {
                std::fs::write(
                    directory.join("asb_resources.py"),
                    helper.replace("'sha256:'+'b'*64", invalid),
                )
                .unwrap();
                assert!(
                    run(&path, true, &mut Vec::new())
                        .unwrap_err()
                        .to_string()
                        .contains("invalid bounded report")
                );
            }
            std::fs::write(directory.join("asb_resources.py"), "import json\nprint(json.dumps({'schema_version':1,'scope':'fake','resources':[]}))\n").unwrap();
            assert!(run(&path, true, &mut Vec::new()).is_err());
        }
    }
}
