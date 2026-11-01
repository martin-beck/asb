//! The shared project Python used to create isolated Python tool environments.

use crate::install::{
    InstallFailure, InstallResult, OperationContext, ProcessScope, ProducerDirectory, Purpose,
};
use crate::{Error, project};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const PYTHON_VERSION_REQUESTS: [&str; 2] = ["3.12", "3.13"];

fn bootstrap_dependency_failure(failure: InstallFailure) -> InstallFailure {
    match failure {
        InstallFailure::Ordinary(error) => Error::Io(format!(
            "could not bootstrap project Python: {error}. Install uv, or install Python 3.12 or 3.13 and retry."
        ))
        .into(),
        abort @ InstallFailure::Abort(_) => abort,
    }
}

fn inspect_in(
    scope: &mut ProcessScope,
    program: &Path,
    arguments: &[&str],
    label: &str,
) -> InstallResult<Option<Vec<u8>>> {
    let Some(program) = crate::tools::host_executable(program) else {
        return Ok(None);
    };
    let mut step = scope.prepare(program, Purpose::Version, label)?;
    step.capture(crate::install::process::Capture::inspection());
    step.command_mut().args(arguments).stdin(Stdio::null());
    let result = scope.run(step)?;
    Ok(result.status.success().then_some(result.stdout))
}

fn version_in(scope: &mut ProcessScope, executable: &Path) -> InstallResult<Option<(u32, u32)>> {
    let Some(bytes) = inspect_in(
        scope,
        executable,
        &[
            "-I",
            "-S",
            "-c",
            "import sys; print('%d.%d' % sys.version_info[:2])",
        ],
        "Python interpreter version",
    )?
    else {
        return Ok(None);
    };
    let Some((major, minor)) = std::str::from_utf8(&bytes)
        .ok()
        .and_then(|text| text.trim().split_once('.'))
    else {
        return Ok(None);
    };
    Ok(major.parse().ok().zip(minor.parse().ok()))
}
fn compatible_in(scope: &mut ProcessScope, executable: &Path) -> InstallResult<bool> {
    Ok(matches!(version_in(scope, executable)?, Some((3, 12 | 13))))
}
pub(crate) fn matches_project_in(
    scope: &mut ProcessScope,
    executable: &Path,
    project_python: &Path,
) -> InstallResult<bool> {
    Ok(compatible_in(scope, executable)?
        && version_in(scope, executable)? == version_in(scope, project_python)?)
}

#[cfg(test)]
fn version(context: &OperationContext, executable: &Path) -> Option<(u32, u32)> {
    let owner = ProducerDirectory::open(context.root()).ok()?;
    let mut scope = owner.scope(context);
    let result = version_in(&mut scope, executable).ok()?;
    owner.consume(scope.finish().ok()?).ok()?;
    result
}
#[cfg(test)]
fn matches_project(context: &OperationContext, executable: &Path, project_python: &Path) -> bool {
    let Ok(owner) = ProducerDirectory::open(context.root()) else {
        return false;
    };
    let mut scope = owner.scope(context);
    let result = matches_project_in(&mut scope, executable, project_python);
    let settled = scope
        .finish()
        .and_then(|permission| owner.consume(permission));
    settled.is_ok() && result.unwrap_or(false)
}

pub(crate) fn project_python(root: &Path) -> PathBuf {
    root.join(".asb/python/venv/bin/python")
}

/// Preserve a previous environment even when it contains manual work.
pub(crate) fn preserve_environment(path: &Path) -> Result<Option<PathBuf>, Error> {
    match fs::symlink_metadata(path) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(Error::Io(format!(
                "cannot inspect {}: {error}",
                path.display()
            )));
        }
    }
    let parent = path.parent().expect("environment has a parent");
    for index in 1..=10_000 {
        let backup = parent.join(format!("venv.asb-backup-{index}"));
        match fs::create_dir(&backup) {
            Ok(()) => {
                let destination = backup.join("venv");
                fs::rename(path, &destination).map_err(|error| {
                    Error::Io(format!(
                        "cannot preserve Python environment {} at {}: {error}",
                        path.display(),
                        destination.display()
                    ))
                })?;
                return Ok(Some(destination));
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(Error::Io(format!(
                    "cannot create environment backup {}: {error}",
                    backup.display()
                )));
            }
        }
    }
    Err(Error::Io(format!(
        "cannot reserve a Python environment backup in {}",
        parent.display()
    )))
}

fn installed_python_in(scope: &mut ProcessScope) -> InstallResult<Option<PathBuf>> {
    for name in ["python3.12", "python3.13", "python3"] {
        let candidate = PathBuf::from(name);
        if compatible_in(scope, &candidate)? {
            return Ok(Some(candidate));
        }
    }
    for request in PYTHON_VERSION_REQUESTS {
        let Some(bytes) = inspect_in(
            scope,
            Path::new("uv"),
            &[
                "python",
                "find",
                "--no-project",
                "--no-config",
                "--no-python-downloads",
                request,
            ],
            "uv managed Python lookup",
        )?
        else {
            continue;
        };
        if let Ok(text) = std::str::from_utf8(&bytes) {
            let path = PathBuf::from(text.trim());
            if path.is_absolute() && compatible_in(scope, &path)? {
                return Ok(Some(path));
            }
        }
    }
    Ok(None)
}

pub(crate) fn ensure_in(
    context: &OperationContext,
    output: &mut dyn std::io::Write,
) -> InstallResult<PathBuf> {
    let root = context.root();
    let owned_context = context.with_destination(&root.join(".asb/python"))?;
    let context = &owned_context;
    let python = project_python(root);
    let probe_owner = ProducerDirectory::open(root)?;
    let mut probe = probe_owner.scope(context);
    if compatible_in(&mut probe, &python)? {
        probe_owner.consume(probe.finish()?)?;
        return Ok(python);
    }
    let interpreter = installed_python_in(&mut probe)?;
    probe_owner.consume(probe.finish()?)?;
    let env_dir = root.join(".asb/python/venv");
    fs::create_dir_all(env_dir.parent().expect("venv parent")).map_err(crate::io_error)?;
    if let Some(backup) = preserve_environment(&env_dir)? {
        writeln!(
            output,
            "Preserved the previous project Python environment at {}.",
            backup.display()
        )
        .map_err(crate::io_error)?;
    }
    let tools_root = project::resolve(root, &context.project().tools_dir);
    if interpreter.is_none() {
        crate::install::ensure_runtime_dependency_in(
            context,
            "uv",
            &tools_root,
            output,
            &mut |_, _| {},
        )
        .map_err(bootstrap_dependency_failure)?;
    }
    // This is a NEW actual directory incarnation, after previous probe proof
    // was consumed and any previous environment was preserved without deletion.
    fs::create_dir(&env_dir).map_err(crate::io_error)?;
    let owner = ProducerDirectory::open(&env_dir)?;
    let mut scope = owner.scope(context);
    let downloads_python = interpreter.is_none();
    let mut step = if let Some(interpreter) = interpreter {
        writeln!(
            output,
            "Preparing the project Python environment using {}.",
            interpreter.display()
        )
        .map_err(crate::io_error)?;
        let mut step = scope.prepare(
            interpreter,
            Purpose::Configure,
            "project Python environment creation",
        )?;
        step.command_mut()
            .args(["-m", "venv", "--without-pip"])
            .arg(&env_dir);
        step
    } else {
        writeln!(
            output,
            "No compatible Python found; using uv to download Python 3.12 for this project."
        )
        .map_err(crate::io_error)?;
        let mut step =
            scope.prepare("uv", Purpose::Package, "project Python bootstrap download")?;
        crate::tools::apply_runtime_path_at(step.command_mut(), &tools_root)?;
        step.command_mut()
            .args(["venv", "--no-project", "--no-config", "--python", "3.12"])
            .arg(&env_dir)
            .env("UV_PYTHON_INSTALL_DIR", root.join(".asb/python/uv-python"))
            .env("UV_CACHE_DIR", root.join(".asb/python/uv-cache"));
        step
    };
    step.command_mut().stdin(Stdio::null());
    let monitor = downloads_python.then(|| {
        crate::install::transfer::Cache::start_many(
            &[&root.join(".asb/python/uv-cache"), &env_dir],
            "Downloading and materializing Python runtime",
        )
    });
    let result = scope.run(step);
    if let Some(monitor) = monitor {
        let _ = monitor.finish();
    }
    let status = result?.status;
    let compatible = compatible_in(&mut scope, &python)?;
    owner.consume(scope.finish()?)?;
    if !status.success() || !compatible {
        return Err(Error::Io(format!("could not prepare a compatible Python 3.12 or 3.13 environment (setup {status}); inspect the installation diagnostics for Python/uv, project-directory access, network or proxy errors, then retry the tool installation")).into());
    }
    Ok(python)
}

pub(crate) fn add_to_path(command: &mut Command, root: &Path, program: &Path) -> Result<(), Error> {
    let python = project_python(root);
    if !python.is_file() {
        return Ok(());
    }
    let path = command
        .get_envs()
        .find(|(name, _)| *name == std::ffi::OsStr::new("PATH"))
        .and_then(|(_, value)| value.map(std::ffi::OsStr::to_os_string))
        .or_else(|| std::env::var_os("PATH"))
        .unwrap_or_default();
    // An installed Python tool must keep its own dependencies visible to
    // subprocesses. It uses the same project-selected Python version.
    let tool_bin = program.parent().filter(|bin| {
        bin.file_name().is_some_and(|name| name == "bin")
            && bin
                .parent()
                .is_some_and(|venv| venv.join("pyvenv.cfg").is_file())
    });
    let joined = std::env::join_paths(
        tool_bin
            .into_iter()
            .map(Path::to_path_buf)
            .chain(std::iter::once(
                python.parent().expect("Python bin").to_path_buf(),
            ))
            .chain(std::env::split_paths(&path)),
    )
    .map_err(|error| Error::Io(format!("cannot configure project Python PATH: {error}")))?;
    command.env("PATH", joined);
    Ok(())
}

#[cfg(test)]
#[path = "../tests/unit/python_runtime_boundaries.rs"]
mod boundary_tests;

#[cfg(all(test, target_os = "linux"))]
#[path = "../tests/unit/python_bootstrap_offline.rs"]
mod bootstrap_offline_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bootstrap_dependency_ordinary_failure_keeps_cause_and_actionable_remedy() {
        let error =
            bootstrap_dependency_failure(Error::Io("offline source unavailable".into()).into());
        let InstallFailure::Ordinary(error) = error else {
            panic!("ordinary bootstrap failure must remain ordinary");
        };
        let message = error.to_string();
        assert!(
            message.contains("could not bootstrap project Python"),
            "{message}"
        );
        assert!(message.contains("offline source unavailable"), "{message}");
        assert!(
            message.contains("Install uv, or install Python 3.12 or 3.13"),
            "{message}"
        );
    }

    #[test]
    fn bootstrap_dependency_abort_preserves_typed_cancellation_and_all_facts() {
        let root = Path::new("/fixture-python-bootstrap");
        let context = OperationContext::new(root, &project::ProjectConfig::default()).unwrap();
        context.cancel();
        let failure = context
            .check_before_admission("uv bootstrap", root)
            .unwrap_err();
        let original = format!("{failure:?}");
        let failure = bootstrap_dependency_failure(failure);
        assert!(matches!(failure, InstallFailure::Abort(_)));
        assert_eq!(format!("{failure:?}"), original);
        assert!(!failure.to_string().contains("Install uv"));
    }

    #[test]
    fn project_python_precedes_existing_child_path() {
        let root = std::env::temp_dir().join(format!("asb-python-path-{}", std::process::id()));
        let bin = root.join(".asb/python/venv/bin");
        fs::create_dir_all(&bin).unwrap();
        fs::write(bin.join("python"), "fixture").unwrap();
        let mut command = Command::new("unused");
        command.env("PATH", "/custom/bin:/usr/bin");
        add_to_path(&mut command, &root, Path::new("/usr/bin/tool")).unwrap();
        let path = command
            .get_envs()
            .find(|(key, _)| *key == "PATH")
            .unwrap()
            .1
            .unwrap();
        let entries: Vec<_> = std::env::split_paths(path).collect();
        assert_eq!(
            entries,
            vec![bin, PathBuf::from("/custom/bin"), PathBuf::from("/usr/bin")]
        );
        let tool_bin = root.join("tool/venv/bin");
        fs::create_dir_all(&tool_bin).unwrap();
        fs::write(root.join("tool/venv/pyvenv.cfg"), "fixture").unwrap();
        let mut python_tool = Command::new("unused");
        python_tool.env("PATH", "/custom/bin");
        add_to_path(&mut python_tool, &root, &tool_bin.join("agent")).unwrap();
        let path = python_tool
            .get_envs()
            .find(|(key, _)| *key == "PATH")
            .unwrap()
            .1
            .unwrap();
        assert_eq!(
            std::env::split_paths(path).collect::<Vec<_>>(),
            vec![
                tool_bin,
                root.join(".asb/python/venv/bin"),
                PathBuf::from("/custom/bin")
            ]
        );
        fs::remove_dir_all(root).unwrap();
    }
}
