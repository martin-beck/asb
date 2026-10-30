//! Single-source registry for every `asb tool install` target.

use crate::install::Recipe;
use crate::project;
use std::path::Path;

pub(crate) mod bpftrace;
mod cli2key;
mod flamegraph;
mod harbor;
mod http;
mod iostat;
mod livecodebench;
mod mpstat;
mod node;
mod perf;
mod perfetto;
pub(crate) mod runtime;
mod swebench;
mod terminal_bench;
mod uv;

#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum ToolKind {
    Collector,
    Support,
}

pub(crate) trait ToolAdapter: Sync {
    fn id(&self) -> &'static str;
    fn recipe(&self) -> Recipe;
    fn helper_executables(&self) -> &'static [&'static str] {
        &[]
    }
    fn runtime_inputs(
        &self,
        context: &crate::runtime_inputs::Context<'_>,
    ) -> Result<Vec<crate::runtime_inputs::Dependency>, crate::Error> {
        let executable =
            resolve(self.id(), context.root, &context.project.tools_dir).ok_or_else(|| {
                crate::Error::Input("selected support-tool runtime is not installed".into())
            })?;
        let mut command = std::process::Command::new(&executable);
        project::apply_python_path(&mut command, context.root, Path::new(&executable))?;
        apply_runtime_path(&mut command, context.root, &context.project.tools_dir)?;
        let mut inputs = self.recipe().runtime_inputs(&context.tool_root(self.id()));
        inputs.push(crate::runtime_inputs::Dependency::command(&command));
        for executable in self.helper_executables() {
            inputs.push(crate::runtime_inputs::Dependency::program(
                context.tool_root(self.id()).join("bin").join(executable),
            ));
        }
        Ok(inputs)
    }
    fn kind(&self) -> ToolKind;
    fn guest_assets(&self, _root: &Path) -> Result<Vec<GuestAsset>, crate::Error> {
        Ok(Vec::new())
    }
    fn runtime_path(&self, _root: &Path) -> Option<std::path::PathBuf> {
        None
    }
    fn host_runtime_usable(
        &self,
        _context: &crate::install::OperationContext,
    ) -> crate::install::InstallResult<bool> {
        Ok(self.executable().is_some_and(host_program_on_path))
    }
    fn post_install(
        &self,
        _context: &crate::install::OperationContext,
        _executable: &Path,
        _progress: &mut dyn FnMut(u8, crate::install::ProgressStage),
        _output: &mut dyn std::io::Write,
    ) -> crate::install::InstallResult<()> {
        Ok(())
    }

    /// Executable published by this tool, when it has one. This keeps the
    /// installer, availability check, and runner on the same catalog entry.
    fn executable(&self) -> Option<&'static str> {
        match self.recipe() {
            Recipe::Npm { executable, .. }
            | Recipe::Python { executable, .. }
            | Recipe::Host { executable }
            | Recipe::Custom { executable, .. } => Some(executable),
        }
    }
}

type PostInstallHook = fn(
    &crate::install::OperationContext,
    &Path,
    &mut dyn FnMut(u8, crate::install::ProgressStage),
    &mut dyn std::io::Write,
) -> crate::install::InstallResult<()>;

#[derive(serde::Serialize)]
pub(crate) struct GuestAsset {
    pub source: std::path::PathBuf,
    pub target: std::path::PathBuf,
    #[serde(skip)]
    pub distribution: Option<(&'static str, &'static str)>,
}
type GuestAssetsHook = fn(&Path) -> Result<Vec<GuestAsset>, crate::Error>;

pub(crate) struct StaticTool {
    id: &'static str,
    recipe: Recipe,
    kind: ToolKind,
    post_install: Option<PostInstallHook>,
    runtime_path: bool,
    host_probe:
        Option<fn(&crate::install::OperationContext) -> crate::install::InstallResult<bool>>,
    guest_assets: Option<GuestAssetsHook>,
    helper_executables: &'static [&'static str],
}
impl StaticTool {
    pub(crate) const fn new(id: &'static str, recipe: Recipe, kind: ToolKind) -> Self {
        Self {
            id,
            recipe,
            kind,
            post_install: None,
            runtime_path: false,
            host_probe: None,
            guest_assets: None,
            helper_executables: &[],
        }
    }
    pub(crate) const fn with_post_install(mut self, hook: PostInstallHook) -> Self {
        self.post_install = Some(hook);
        self
    }
    pub(crate) const fn with_runtime_path(mut self) -> Self {
        self.runtime_path = true;
        self
    }
    pub(crate) const fn with_helper_executables(mut self, names: &'static [&'static str]) -> Self {
        self.helper_executables = names;
        self
    }
    pub(crate) const fn with_host_probe(
        mut self,
        probe: fn(&crate::install::OperationContext) -> crate::install::InstallResult<bool>,
    ) -> Self {
        self.host_probe = Some(probe);
        self
    }
    pub(crate) const fn with_guest_assets(mut self, hook: GuestAssetsHook) -> Self {
        self.guest_assets = Some(hook);
        self
    }
}
impl ToolAdapter for StaticTool {
    fn helper_executables(&self) -> &'static [&'static str] {
        self.helper_executables
    }
    fn guest_assets(&self, root: &Path) -> Result<Vec<GuestAsset>, crate::Error> {
        self.guest_assets
            .map_or_else(|| Ok(Vec::new()), |hook| hook(root))
    }
    fn id(&self) -> &'static str {
        self.id
    }
    fn recipe(&self) -> Recipe {
        self.recipe
    }
    fn kind(&self) -> ToolKind {
        self.kind
    }
    fn runtime_path(&self, root: &Path) -> Option<std::path::PathBuf> {
        self.runtime_path
            .then(|| root.join("bin"))
            .filter(|path| path.is_dir())
    }
    fn host_runtime_usable(
        &self,
        context: &crate::install::OperationContext,
    ) -> crate::install::InstallResult<bool> {
        if let Some(probe) = self.host_probe {
            return probe(context);
        }
        Ok(self.executable().is_some_and(host_program_on_path))
    }
    fn post_install(
        &self,
        context: &crate::install::OperationContext,
        executable: &Path,
        progress: &mut dyn FnMut(u8, crate::install::ProgressStage),
        output: &mut dyn std::io::Write,
    ) -> crate::install::InstallResult<()> {
        self.post_install
            .map_or(Ok(()), |hook| hook(context, executable, progress, output))
    }
}

fn host_program_on_path(program: &str) -> bool {
    host_executable(Path::new(program)).is_some()
}

/// Resolve the same first executable PATH entry as Command without launching
/// it. Preserve symlinks so input binding can detect later retargeting.
pub(crate) fn host_executable(program: &Path) -> Option<std::path::PathBuf> {
    host_executable_in(program, std::env::var_os("PATH").as_deref())
}

fn host_executable_in(
    program: &Path,
    search: Option<&std::ffi::OsStr>,
) -> Option<std::path::PathBuf> {
    executable_candidates(program, search)?
        .into_iter()
        .find(|path| crate::executables::is_executable(path))
        .and_then(|path| std::path::absolute(path).ok())
}

/// Availability lookup at the command's captured cwd, without UTF-8 conversion
/// or canonicalizing symlink identity. This is not proof of later exec success.
pub(crate) fn host_executable_in_at(
    program: &Path,
    search: Option<&std::ffi::OsStr>,
    cwd: &Path,
) -> Option<std::path::PathBuf> {
    executable_candidates(program, search)?
        .into_iter()
        .map(|path| {
            if path.is_absolute() {
                path
            } else {
                cwd.join(path)
            }
        })
        .find(|path| crate::executables::is_executable(path))
}

fn executable_candidates(
    program: &Path,
    search: Option<&std::ffi::OsStr>,
) -> Option<Vec<std::path::PathBuf>> {
    if program.as_os_str().is_empty() {
        return None;
    }
    Some(
        if program.components().count() > 1 || program.is_absolute() {
            vec![program.to_path_buf()]
        } else {
            std::env::split_paths(search?)
                .map(|directory| directory.join(program))
                .collect()
        },
    )
}

static REGISTRY: &[&dyn ToolAdapter] = &[
    &node::ADAPTER,
    &uv::ADAPTER,
    &cli2key::ADAPTER,
    &mpstat::ADAPTER,
    &iostat::ADAPTER,
    &perf::ADAPTER,
    &bpftrace::ADAPTER,
    &perfetto::ADAPTER,
    &flamegraph::ADAPTER,
    &livecodebench::ADAPTER,
    &swebench::ADAPTER,
    &terminal_bench::ADAPTER,
    &harbor::ADAPTER,
    &http::ADAPTER,
];
pub(crate) fn all() -> &'static [&'static dyn ToolAdapter] {
    REGISTRY
}
pub(crate) fn find(id: &str) -> Option<&'static dyn ToolAdapter> {
    REGISTRY.iter().copied().find(|tool| tool.id() == id)
}
pub(crate) fn names() -> impl Iterator<Item = &'static str> {
    REGISTRY.iter().map(|tool| tool.id())
}

pub(crate) fn apply_runtime_path(
    command: &mut std::process::Command,
    root: &Path,
    tools_dir: &str,
) -> Result<(), crate::Error> {
    let tools = project::resolve(root, tools_dir);
    apply_runtime_path_at(command, &tools)
}

pub(crate) fn apply_runtime_path_at(
    command: &mut std::process::Command,
    tools: &Path,
) -> Result<(), crate::Error> {
    let original = command
        .get_envs()
        .find(|(name, _)| *name == "PATH")
        .map(|(_, value)| value.map(std::ffi::OsStr::to_os_string))
        .unwrap_or_else(|| std::env::var_os("PATH"));
    if let Some(search) = runtime_path_override_at(tools, original.as_deref())? {
        command.env("PATH", search);
    }
    Ok(())
}

/// None means no local runtime bins: preserve the caller's exact inherited,
/// assigned or removed PATH rather than introducing an override.
pub(crate) fn runtime_path_override_at(
    tools: &Path,
    original: Option<&std::ffi::OsStr>,
) -> Result<Option<std::ffi::OsString>, crate::Error> {
    let mut paths = REGISTRY
        .iter()
        .filter_map(|tool| tool.runtime_path(&tools.join(tool.id())))
        .collect::<Vec<_>>();
    if paths.is_empty() {
        return Ok(None);
    }
    if let Some(original) = original {
        paths.extend(std::env::split_paths(&original));
    }
    std::env::join_paths(paths)
        .map(Some)
        .map_err(|error| crate::Error::Input(format!("invalid runtime PATH: {error}")))
}

/// Resolve a tool executable through its registered project-local layout.
pub(crate) fn local_lookup(id: &str, tools_root: &Path) -> crate::executables::Lookup {
    let Some(tool) = find(id) else {
        return crate::executables::Lookup::Missing;
    };
    let Some(executable) = tool.executable() else {
        return crate::executables::Lookup::Missing;
    };
    crate::executables::project_lookup(tools_root, tool.id(), executable, std::iter::empty())
}

pub(crate) fn resolve(id: &str, project_root: &Path, tools_dir: &str) -> Option<String> {
    local_lookup(id, &project::resolve(project_root, tools_dir))
        .path()
        .map(|path| path.to_string_lossy().into_owned())
}

#[cfg(test)]
#[path = "../../tests/unit/tools.rs"]
mod coverage_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn explicit_cwd_lookup_preserves_raw_relative_and_empty_path_entries() {
        use std::os::unix::ffi::OsStringExt;
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(runtime::unique_id());
        let raw = std::ffi::OsString::from_vec(b"raw-\xff".to_vec());
        std::fs::create_dir_all(root.join(&raw)).unwrap();
        let program = root.join(&raw).join("npm");
        // Never executed: this fixture tests availability/path identity only.
        std::fs::write(&program, b"inert").unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(
            host_executable_in_at(Path::new("npm"), Some(&raw), &root),
            Some(program.clone())
        );
        assert_eq!(
            host_executable_in_at(&std::path::PathBuf::from(&raw).join("npm"), None, &root),
            Some(program)
        );
        let here = root.join("npm");
        std::fs::write(&here, b"inert").unwrap();
        std::fs::set_permissions(&here, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(
            host_executable_in_at(Path::new("npm"), Some(std::ffi::OsStr::new("")), &root),
            Some(here)
        );
        assert!(host_executable_in_at(Path::new("npm"), None, &root).is_none());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn absent_local_runtime_bins_preserve_inherited_assigned_and_removed_path() {
        let root = std::env::temp_dir().join(runtime::unique_id());
        std::fs::create_dir_all(&root).unwrap();
        for assigned in [None, Some(Some("relative::/bin")), Some(None)] {
            let mut command = std::process::Command::new("unused");
            match assigned {
                None => {}
                Some(Some(path)) => {
                    command.env("PATH", path);
                }
                Some(None) => {
                    command.env_remove("PATH");
                }
            }
            let before: Vec<_> = command
                .get_envs()
                .map(|(k, v)| (k.to_owned(), v.map(std::ffi::OsStr::to_owned)))
                .collect();
            apply_runtime_path_at(&mut command, &root).unwrap();
            let after: Vec<_> = command
                .get_envs()
                .map(|(k, v)| (k.to_owned(), v.map(std::ffi::OsStr::to_owned)))
                .collect();
            assert_eq!(before, after);
        }
        assert!(runtime_path_override_at(&root, None).unwrap().is_none());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn host_input_resolution_preserves_path_precedence_permissions_and_symlinks() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let root = std::env::temp_dir()
            .join(crate::workloads::TaskIdentity::local("host-input", None).attempt_id);
        let first = root.join("first");
        let second = root.join("second");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        let program = second.join("fixture-program");
        std::fs::write(&program, "#!/bin/sh\nexit 99\n").unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::write(first.join("fixture-program"), "not executable").unwrap();
        std::fs::set_permissions(
            first.join("fixture-program"),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        let search = std::env::join_paths([&first, &second]).unwrap();
        assert_eq!(
            host_executable_in(Path::new("fixture-program"), Some(&search)),
            Some(program.clone())
        );
        std::fs::remove_file(first.join("fixture-program")).unwrap();
        symlink(&program, first.join("fixture-program")).unwrap();
        assert_eq!(
            host_executable_in(Path::new("fixture-program"), Some(&search)),
            Some(first.join("fixture-program"))
        );
        assert_eq!(host_executable_in(&program, None), Some(program));
        assert!(host_executable_in(Path::new("missing"), Some(&search)).is_none());
        assert!(host_executable_in(Path::new("fixture-program"), None).is_none());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn official_benchmark_harnesses_have_pinned_project_local_python_recipes() {
        for (id, expected_binary, expected_package, expected_dependencies) in [
            (
                "swebench",
                "swebench",
                "swebench @ git+https://github.com/SWE-bench/SWE-bench.git@02e7a74ffd0b707aab73d203fe87bdc7c76afc8e",
                &["docker==7.1.0"][..],
            ),
            (
                "terminal-bench",
                "tb",
                "terminal-bench==0.2.18",
                &["docker==7.1.0"][..],
            ),
            (
                "livecodebench",
                "datasets-cli",
                "datasets==3.5.0",
                &["numpy==2.2.4", "tqdm==4.67.1"][..],
            ),
            ("harbor", "harbor", "harbor==0.24.0", &["docker==7.1.0"][..]),
        ] {
            let adapter = find(id).unwrap();
            assert!(adapter.kind() == ToolKind::Support);
            assert_eq!(adapter.executable(), Some(expected_binary));
            match adapter.recipe() {
                Recipe::Python {
                    package,
                    dependencies,
                    executable,
                } => {
                    assert_eq!(package, expected_package);
                    assert_eq!(executable, expected_binary);
                    assert_eq!(dependencies, expected_dependencies);
                }
                _ => panic!("{id} must install the actual official Python harness"),
            }
        }
    }
}
