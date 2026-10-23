use super::{StaticTool, ToolKind};
use crate::Error;
use crate::install::{
    InstallFailure, InstallResult, OperationContext, ProcessScope, ProducerDirectory, Purpose,
    Recipe, command_output_in, install_verified_source_in, make_executable,
};
use std::{
    fs,
    path::{Path, PathBuf},
};

// Kernel.org's standalone perf source release. The digest is pinned from its
// published sha256sums.asc; no kernel build or host package installation is required.
const SOURCE: &str =
    "https://cdn.kernel.org/pub/linux/kernel/tools/perf/v6.12.0/perf-6.12.0.tar.gz";
const SOURCE_SHA256: &str = "e6c5bbaa1d7a80d8bb4ebd43aa15f2027b308a4cd2bdac9ba303e0e65f17081a";

pub(super) static ADAPTER: StaticTool = StaticTool::new(
    "perf",
    Recipe::Custom {
        executable: "perf",
        description: "install a distribution perf package or verified upstream source build",
        summary: "project-local Linux perf",
        install: install_perf,
        guest_assets: None,
    },
    ToolKind::Collector,
);

fn install_perf(
    context: &OperationContext,
    root: &Path,
    progress: &mut dyn FnMut(u8, crate::install::ProgressStage),
) -> InstallResult<PathBuf> {
    if !cfg!(target_os = "linux") {
        return Err(Error::Input("perf requires Linux".into()).into());
    }
    fs::create_dir_all(root).map_err(crate::io_error)?;
    let stage = root.join(format!(".install-{}", super::runtime::unique_id()));
    fs::create_dir(&stage).map_err(crate::io_error)?;
    let owner = ProducerDirectory::open(&stage)?;
    let mut scope = owner.scope(context);
    let prepared = (|| -> InstallResult<(PathBuf, PathBuf)> {
        progress(20, crate::install::ProgressStage::Downloading);
        let source = match distribution_perf(&mut scope, &stage) {
            Ok(path) => path,
            Err(InstallFailure::Abort(abort)) => return Err(InstallFailure::Abort(abort)),
            Err(InstallFailure::Ordinary(package_error)) => {
                progress(35, crate::install::ProgressStage::Building);
                build_perf(context, &mut scope, &stage, progress).map_err(|failure| match failure {
                    InstallFailure::Abort(abort) => InstallFailure::Abort(abort),
                    InstallFailure::Ordinary(build_error) => InstallFailure::Ordinary(Error::Io(format!(
                        "could not obtain a working distribution perf: {package_error}\nCould not build upstream perf: {build_error}\nInstall the compiler, make, flex, bison, libelf and libssl development packages, curl and tar, then retry `asb tool install perf`. Inspect the installation diagnostics for build details."))),
                })?
            }
        };
        let bin = root.join("bin");
        fs::create_dir_all(&bin).map_err(crate::io_error)?;
        let staged_binary = bin.join(format!(".perf-{}", super::runtime::unique_id()));
        fs::copy(&source, &staged_binary).map_err(|error| {
            Error::Io(format!(
                "cannot copy perf from {}: {error}",
                source.display()
            ))
        })?;
        make_executable(&staged_binary)?;
        if !runnable(&mut scope, &staged_binary)? {
            return Err(Error::Io("the project-local perf executable could not run; check its runtime library dependencies".into()).into());
        }
        Ok((staged_binary, bin.join("perf")))
    })();
    // Unknown cleanup is never a package fallback, runnable=false, publication
    // or scratch-removal permission. Leave both stage and exposed binary intact.
    if let Err(InstallFailure::Abort(abort)) = prepared {
        return Err(InstallFailure::Abort(abort));
    }
    owner.consume(scope.finish()?)?;
    let result = match prepared {
        Ok((staged, destination)) => {
            let result = fs::rename(&staged, &destination).map_err(|error| {
                Error::Io(format!(
                    "cannot install perf at {}: {error}",
                    destination.display()
                ))
            });
            if result.is_err() {
                let _ = fs::remove_file(&staged);
            }
            result.map(|()| destination).map_err(Into::into)
        }
        Err(InstallFailure::Ordinary(error)) => Err(InstallFailure::Ordinary(error)),
        Err(InstallFailure::Abort(_)) => unreachable!("abort returned before settlement"),
    };
    let _ = fs::remove_dir_all(&stage);
    result
}

fn runnable(scope: &mut ProcessScope, path: &Path) -> InstallResult<bool> {
    let mut step = scope.prepare(path, Purpose::Version, "perf version")?;
    step.capture(crate::install::process::Capture::inspection());
    step.command_mut()
        .arg("--version")
        .stdin(std::process::Stdio::null());
    Ok(scope.run(step)?.status.success())
}

// APT validates downloads against its configured package indexes; this does not
// introduce any ASB authorization gate or require network in offline fixtures.
fn distribution_perf(scope: &mut ProcessScope, stage: &Path) -> InstallResult<PathBuf> {
    let kernel = command_output_in(scope, Path::new("uname"), &["-r"], "kernel release")?;
    if !kernel
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || b".-_+".contains(&byte))
    {
        return Err(Error::Io("invalid kernel release".into()).into());
    }
    let meta = format!("linux-tools-{kernel}");
    let mut packages = vec![meta.clone()];
    if super::host_executable(Path::new("apt-cache")).is_some() {
        let mut step = scope.prepare("apt-cache", Purpose::Probe, "perf package dependencies")?;
        step.capture(crate::install::process::Capture::inspection());
        step.command_mut()
            .args(["depends", &meta])
            .stdin(std::process::Stdio::null());
        let output = scope.run(step)?;
        if output.status.success() {
            for line in String::from_utf8_lossy(&output.stdout).lines() {
                if let Some(name) = line.trim().strip_prefix("Depends: ")
                    && name.starts_with("linux-")
                    && name.contains("tools")
                    && name
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || b".-+".contains(&byte))
                {
                    packages.insert(0, name.to_owned());
                }
            }
        }
    }
    packages.push("linux-perf".into());
    for (index, package) in packages.iter().enumerate() {
        let directory = stage.join(format!("package-{index}"));
        fs::create_dir(&directory).map_err(crate::io_error)?;
        let mut step = scope.prepare(
            "apt-get",
            Purpose::Download,
            "perf distribution package download",
        )?;
        step.command_mut()
            .args(["download", package])
            .current_dir(&directory)
            .stdin(std::process::Stdio::null());
        if !scope.run(step)?.status.success() {
            continue;
        }
        let archive = fs::read_dir(&directory)
            .map_err(crate::io_error)?
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .find(|path| path.extension().is_some_and(|extension| extension == "deb"));
        let Some(archive) = archive else { continue };
        let extracted = directory.join("extracted");
        let mut step = scope.prepare(
            "dpkg-deb",
            Purpose::Extract,
            "perf distribution package extraction",
        )?;
        step.command_mut()
            .arg("-x")
            .arg(archive)
            .arg(&extracted)
            .stdin(std::process::Stdio::null());
        if !scope.run(step)?.status.success() {
            continue;
        }
        if let Some(found) = find_perf(scope, &extracted)? {
            return Ok(found);
        }
    }
    Err(Error::Io("no runnable perf was found in the configured APT repositories".into()).into())
}

fn find_perf(scope: &mut ProcessScope, directory: &Path) -> InstallResult<Option<PathBuf>> {
    let Ok(entries) = fs::read_dir(directory) else {
        return Ok(None);
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_file() && entry.file_name() == "perf" && runnable(scope, &path)? {
            return Ok(Some(path));
        }
        if kind.is_dir()
            && let Some(found) = find_perf(scope, &path)?
        {
            return Ok(Some(found));
        }
    }
    Ok(None)
}

fn build_perf(
    context: &OperationContext,
    scope: &mut ProcessScope,
    stage: &Path,
    progress: &mut dyn FnMut(u8, crate::install::ProgressStage),
) -> InstallResult<PathBuf> {
    build_perf_from_source(context, scope, stage, progress, SOURCE, SOURCE_SHA256)
}

fn build_perf_from_source(
    context: &OperationContext,
    scope: &mut ProcessScope,
    stage: &Path,
    progress: &mut dyn FnMut(u8, crate::install::ProgressStage),
    location: &str,
    checksum: &str,
) -> InstallResult<PathBuf> {
    let archive = stage.join("perf.tar.gz");
    install_verified_source_in(
        context,
        location,
        &archive,
        "Linux perf source",
        checksum,
        false,
        |_, _| Ok(()),
    )?;
    let source = stage.join("source");
    fs::create_dir(&source).map_err(crate::io_error)?;
    let mut step = scope.prepare("tar", Purpose::Extract, "perf source extraction")?;
    step.command_mut()
        .arg("-xzf")
        .arg(&archive)
        .arg("-C")
        .arg(&source)
        .arg("--strip-components=1")
        .stdin(std::process::Stdio::null());
    if !scope.run(step)?.status.success() {
        return Err(Error::Io("could not extract perf source".into()).into());
    }
    progress(50, crate::install::ProgressStage::Building);
    let mut traceevent = false;
    if super::host_executable(Path::new("pkg-config")).is_some() {
        let mut step = scope.prepare(
            "pkg-config",
            Purpose::Probe,
            "perf libtraceevent availability",
        )?;
        step.command_mut()
            .args(["--exists", "libtraceevent"])
            .stdin(std::process::Stdio::null());
        traceevent = scope.run(step)?.status.success();
    }
    let mut step = scope.prepare("make", Purpose::Build, "perf compilation")?;
    step.command_mut()
        .arg("-C")
        .arg(source.join("tools/perf"))
        .args([
            "JOBS=2",
            "NO_GTK2=1",
            "NO_PERL=1",
            "NO_LIBPYTHON=1",
            "PYTHON=python3",
            "PYTHON_CONFIG=true",
            "NO_LIBAUDIT=1",
            "NO_LIBBABELTRACE=1",
            "NO_LIBNUMA=1",
            "NO_SLANG=1",
        ])
        .stdin(std::process::Stdio::null());
    if !traceevent {
        step.command_mut().arg("NO_LIBTRACEEVENT=1");
    }
    if !scope.run(step)?.status.success() {
        return Err(Error::Io("perf compilation failed".into()).into());
    }
    let binary = source.join("tools/perf/perf");
    if !runnable(scope, &binary)? {
        return Err(Error::Io("built perf is not runnable".into()).into());
    }
    Ok(binary)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "downloads a distribution package from configured repositories"]
    fn live_distribution_package_is_runnable() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target")
            .join(format!("asb-perf-package-test-{}", std::process::id()));
        fs::create_dir(&root).unwrap();
        let context =
            OperationContext::new(&root, &crate::project::ProjectConfig::default()).unwrap();
        let owner = ProducerDirectory::open(&root).unwrap();
        let mut scope = owner.scope(&context);
        let result = distribution_perf(&mut scope, &root);
        let valid = match &result {
            Ok(path) => runnable(&mut scope, path).unwrap(),
            Err(_) => false,
        };
        owner.consume(scope.finish().unwrap()).unwrap();
        assert!(valid, "{result:?}");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[ignore = "downloads and compiles upstream perf"]
    fn live_upstream_source_build_is_runnable() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target")
            .join(format!("asb-perf-source-test-{}", std::process::id()));
        fs::create_dir(&root).unwrap();
        let context =
            OperationContext::new(&root, &crate::project::ProjectConfig::default()).unwrap();
        let owner = ProducerDirectory::open(&root).unwrap();
        let mut scope = owner.scope(&context);
        let result = build_perf(&context, &mut scope, &root, &mut |_, _| {});
        let valid = match &result {
            Ok(path) => runnable(&mut scope, path).unwrap(),
            Err(_) => false,
        };
        owner.consume(scope.finish().unwrap()).unwrap();
        assert!(valid, "{result:?}");
        fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(all(test, unix))]
#[path = "../../tests/unit/perf_source_install.rs"]
mod source_install_tests;
