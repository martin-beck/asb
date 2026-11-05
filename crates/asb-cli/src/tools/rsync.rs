//! Build project-local rsync with the ACL and xattr support snapshots require.
use super::{StaticTool, ToolKind};
use crate::{
    Error,
    install::{InstallResult, OperationContext, ProducerDirectory, ProgressStage, Purpose, Recipe},
};
use std::path::{Path, PathBuf};
use std::process::Stdio;

pub(super) static ADAPTER: StaticTool = StaticTool::new(
    "rsync",
    Recipe::Custom {
        executable: "rsync",
        description: "build project-local rsync with ACL and xattr support",
        summary: "rsync 3.5.1",
        install,
        guest_assets: None,
    },
    ToolKind::Support,
)
.with_init_runtime()
.with_runtime_path()
.with_host_probe(host_usable);

fn host_usable(context: &OperationContext) -> InstallResult<bool> {
    let Some(path) = super::host_executable(Path::new("rsync")) else {
        return Ok(false);
    };
    let owner = ProducerDirectory::open(context.root())?;
    let mut scope = owner.scope(context);
    let mut step = scope.prepare(path, Purpose::Probe, "rsync ACL and xattr probe")?;
    step.command_mut()
        .args(["-AX", "--version"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let usable = scope.run(step)?.status.success();
    owner.consume(scope.finish()?)?;
    Ok(usable)
}

const SOURCES: [(&str, &str, &str); 3] = [
    (
        "attr-2.5.2",
        "https://download-mirror.savannah.gnu.org/releases/attr/attr-2.5.2.tar.gz",
        "39bf67452fa41d0948c2197601053f48b3d78a029389734332a6309a680c6c87",
    ),
    (
        "acl-2.3.2",
        "https://download-mirror.savannah.gnu.org/releases/acl/acl-2.3.2.tar.gz",
        "5f2bdbad629707aa7d85c623f994aa8a1d2dec55a73de5205bac0bf6058a2f7c",
    ),
    (
        "rsync-3.5.1",
        "https://github.com/RsyncProject/rsync/releases/download/v3.5.1/rsync-3.5.1.tar.gz",
        "c55f9c9dc10fb8bec397b399a0fdded53cc9a2d8e30891bb0d63724d25c37bef",
    ),
];

fn install(
    context: &OperationContext,
    root: &Path,
    progress: &mut dyn FnMut(u8, ProgressStage),
) -> InstallResult<PathBuf> {
    if !cfg!(target_os = "linux") {
        return Err(Error::Input(
            "project-local rsync source build currently requires Linux".into(),
        )
        .into());
    }
    let root = std::path::absolute(root).map_err(crate::io_error)?;
    let build = root.join(format!("build-{}", super::runtime::unique_id()));
    let deps = build.join("deps");
    std::fs::create_dir_all(&build).map_err(crate::io_error)?;
    let mut archives = Vec::new();
    for (name, url, checksum) in SOURCES {
        let path = root.join("cache").join(format!("{name}.tar.gz"));
        archives.push(super::runtime::cached_archive_in(
            context, &path, url, checksum, progress,
        )?);
    }
    let owner = ProducerDirectory::open(&root)?;
    let mut scope = owner.scope(context);
    progress(40, ProgressStage::Building);
    for archive in archives {
        let mut unpack = scope.prepare("tar", Purpose::Extract, "rsync source extraction")?;
        unpack
            .command_mut()
            .arg("-xzf")
            .arg(archive)
            .arg("-C")
            .arg(&build)
            .stdin(Stdio::null());
        if !scope.run(unpack)?.status.success() {
            return Err(Error::Io("could not extract rsync build sources".into()).into());
        }
    }
    let includes = format!("-I{}", deps.join("include").display());
    let libraries = format!("-L{}", deps.join("lib").display());
    for (name, _, _) in SOURCES {
        let source = build.join(name);
        let mut configure = scope.prepare(
            source.join("configure"),
            Purpose::Build,
            "rsync dependency configuration",
        )?;
        configure
            .command_mut()
            .current_dir(&source)
            .env("CPPFLAGS", &includes)
            .env("LDFLAGS", &libraries)
            .stdin(Stdio::null());
        if name.starts_with("rsync-") {
            configure.command_mut().args([
                "--disable-openssl",
                "--disable-xxhash",
                "--disable-zstd",
                "--disable-lz4",
                "--disable-md2man",
                "--disable-idn",
            ]);
        } else {
            configure
                .command_mut()
                .arg(format!("--prefix={}", deps.display()))
                .args(["--enable-static", "--disable-shared", "--disable-nls"]);
        }
        if !scope.run(configure)?.status.success() {
            return Err(Error::Io(format!(
                "could not configure {name}; ensure a C compiler and build tools are available"
            ))
            .into());
        }
        let mut make = scope.prepare("make", Purpose::Build, "rsync source build")?;
        make.command_mut()
            .current_dir(&source)
            .arg("-j2")
            .stdin(Stdio::null());
        if !scope.run(make)?.status.success() {
            return Err(Error::Io(format!(
                "could not build {name}; inspect installation diagnostics"
            ))
            .into());
        }
        if !name.starts_with("rsync-") {
            let mut install =
                scope.prepare("make", Purpose::Build, "rsync dependency installation")?;
            install
                .command_mut()
                .current_dir(&source)
                .arg("install")
                .stdin(Stdio::null());
            if !scope.run(install)?.status.success() {
                return Err(Error::Io(format!("could not stage {name}")).into());
            }
        }
    }
    let candidate = build.join("rsync-3.5.1/rsync");
    let mut verify = scope.prepare(&candidate, Purpose::Probe, "rsync ACL and xattr validation")?;
    verify
        .command_mut()
        .args(["-AX", "--version"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if !scope.run(verify)?.status.success() {
        return Err(Error::Io("built rsync lacks required ACL or xattr support".into()).into());
    }
    owner.consume(scope.finish()?)?;
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin).map_err(crate::io_error)?;
    crate::install::install_executable_source_in(
        context,
        candidate
            .to_str()
            .ok_or_else(|| Error::Input("rsync build path is not valid UTF-8".into()))?,
        &bin.join("rsync"),
        "rsync",
    )?;
    progress(100, ProgressStage::Ready);
    Ok(bin.join("rsync"))
}
