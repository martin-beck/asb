//! Shared pinned original source archive for all three SWE-bench populations.
use super::SweBenchAdapter;
use crate::Error;
use crate::install::{
    InstallResult, ProcessScope, ProducerDirectory, Purpose, VerifiedSource,
    install_verified_source_bounded_in,
};
use crate::workloads::{InstallationContext, TaskIdentity};
use std::fs;
use std::io::Write;
use std::path::Path;

const ARCHIVE: &str = "swe-bench-tasks-3d07b464b7b311a0cbfb5ed5b2d8a3b96f84a33d.tar.gz";
const URL: &str = "https://codeload.github.com/SWE-bench/swe-bench-tasks/tar.gz/3d07b464b7b311a0cbfb5ed5b2d8a3b96f84a33d";

pub(super) fn install(
    adapter: &SweBenchAdapter,
    context: &InstallationContext<'_>,
    output: &mut dyn Write,
    progress: &mut dyn FnMut(u8, crate::install::ProgressStage),
) -> InstallResult<()> {
    let tool = context.tools_root.join("swebench");
    let catalog = context.workloads_root.join(adapter.id).join("catalog.json");
    let owner = ProducerDirectory::open(&tool)?;
    let mut scope = owner.scope(context.process);
    let verify = |scope: &mut ProcessScope,
                  archive: &Path,
                  catalog: Option<&Path>|
     -> InstallResult<bool> {
        let python = tool.join("venv/bin/python");
        #[cfg(target_os = "linux")]
        let python = {
            let python = std::path::absolute(python).map_err(crate::io_error)?;
            context
                .process
                .check_before_admission("original SWE-bench source validation", &python)?;
            let selected = crate::tools::host_executable(&python);
            context
                .process
                .check_before_admission("original SWE-bench source validation", &python)?;
            selected.ok_or_else(|| Error::Io(format!(
                    "cannot validate original SWE-bench source: installed evaluator runtime {} is not currently executable; run `asb tool install swebench`",
                    python.display()
                )))?
        };
        let mut step = scope.prepare(
            python,
            Purpose::Probe,
            "original SWE-bench source validation",
        )?;
        step.command_mut()
            .arg(tool.join("asb_swebench_sources.py"))
            .arg("--archive")
            .arg(archive)
            .env("PYTHONDONTWRITEBYTECODE", "1");
        if let Some(catalog) = catalog {
            step.command_mut().arg("--catalog").arg(catalog);
        }
        Ok(scope.run(step)?.status.success())
    };
    let destination = tool.join(ARCHIVE);
    if destination.is_file() && verify(&mut scope, &destination, Some(&catalog))? {
        owner.consume(scope.finish()?)?;
        return Ok(());
    }
    let cache = context.project_root.join(".asb-cache/workload-sources");
    fs::create_dir_all(&cache).map_err(crate::io_error)?;
    let archive = cache.join(ARCHIVE);
    if fs::symlink_metadata(&archive).is_ok() && !verify(&mut scope, &archive, None)? {
        preserve(&archive, output)?;
    }
    if !archive.is_file() {
        progress(80, crate::install::ProgressStage::Downloading);
        let temporary = archive.with_extension(format!(
            "part-{}",
            TaskIdentity::local(adapter.id, None).attempt_id
        ));
        install_verified_source_bounded_in(
            context.process,
            URL,
            &temporary,
            VerifiedSource {
                kind: "original SWE-bench Dockerfiles",
                checksum: adapter.source_sha256,
                executable: false,
                limit: 64 * 1024 * 1024,
            },
            |_, _| Ok(()),
        )?;
        if !verify(&mut scope, &temporary, None)? {
            return Err(Error::Input("original SWE-bench source download failed or did not match its pinned checksum; inspect installation diagnostics, check network/proxy access and retry workload installation".into()).into());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600))
                .map_err(crate::io_error)?;
        }
        fs::rename(&temporary, &archive).map_err(crate::io_error)?;
    }
    progress(90, crate::install::ProgressStage::Validating);
    if !verify(&mut scope, &archive, Some(&catalog))? {
        return Err(Error::Input("original SWE-bench source definitions do not match the installed dataset; reinstall the pinned workload and inspect installation diagnostics".into()).into());
    }
    owner.consume(scope.finish()?)?;
    if fs::symlink_metadata(&destination).is_ok() {
        preserve(&destination, output)?;
    }
    let temporary = destination.with_extension(format!(
        "part-{}",
        TaskIdentity::local(adapter.id, None).attempt_id
    ));
    fs::copy(&archive, &temporary).map_err(crate::io_error)?;
    fs::rename(&temporary, &destination).map_err(crate::io_error)?;
    Ok(())
}

fn preserve(path: &Path, output: &mut dyn Write) -> Result<(), Error> {
    let metadata = fs::symlink_metadata(path).map_err(crate::io_error)?;
    if !metadata.is_file() {
        return Err(Error::Input(format!(
            "official task source {} must be a regular file, not a symlink or directory",
            path.display()
        )));
    }
    let backup = path.with_extension(format!(
        "asb-backup-{}",
        TaskIdentity::local("swebench-source", None).attempt_id
    ));
    fs::rename(path, &backup).map_err(crate::io_error)?;
    writeln!(
        output,
        "Preserved the changed original task archive at {} before reinstalling.",
        backup.display()
    )
    .map_err(crate::io_error)
}

#[cfg(test)]
#[path = "sources/coverage_tests.rs"]
mod coverage_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_pins_match_the_published_helper_contract() {
        let helper = include_str!("../../../../assets/benchmarks/asb_swebench_sources.py");
        let revision = ARCHIVE
            .strip_prefix("swe-bench-tasks-")
            .unwrap()
            .strip_suffix(".tar.gz")
            .unwrap();
        assert!(URL.ends_with(revision));
        assert!(helper.contains(&format!("SOURCE_REVISION = \"{revision}\"")));
        assert!(helper.contains(&format!(
            "SOURCE_SHA256 = \"{}\"",
            super::super::ADAPTER.source_sha256
        )));
    }

    #[test]
    fn changed_source_archives_are_preserved_without_overwriting_their_contents() {
        let root = std::env::temp_dir().join(TaskIdentity::local("source-backup", None).attempt_id);
        fs::create_dir_all(&root).unwrap();
        let archive = root.join("source.tar.gz");
        fs::write(&archive, "changed source evidence").unwrap();
        let mut output = Vec::new();
        preserve(&archive, &mut output).unwrap();
        assert!(!archive.exists());
        let backups: Vec<_> = fs::read_dir(&root)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        assert_eq!(backups.len(), 1);
        assert_eq!(
            fs::read_to_string(&backups[0]).unwrap(),
            "changed source evidence"
        );
        assert!(
            String::from_utf8(output)
                .unwrap()
                .contains("Preserved the changed")
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn source_backup_does_not_follow_symlinks_or_move_directories() {
        let root = std::env::temp_dir().join(TaskIdentity::local("source-link", None).attempt_id);
        fs::create_dir_all(root.join("directory")).unwrap();
        let target = root.join("unrelated");
        fs::write(&target, "untouched").unwrap();
        let link = root.join("linked.tar.gz");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        for path in [&link, &root.join("directory")] {
            assert!(preserve(path, &mut Vec::new()).is_err());
            assert!(fs::symlink_metadata(path).is_ok());
        }
        assert_eq!(fs::read_to_string(target).unwrap(), "untouched");
        fs::remove_dir_all(root).unwrap();
    }
}
