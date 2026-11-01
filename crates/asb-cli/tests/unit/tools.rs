//! Generic dispatch and path declarations do not execute registered tools.
use super::*;
use std::fs;
use std::path::PathBuf;
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/tools-registry-fixtures")
            .join(crate::workloads::TaskIdentity::local("fixture", None).attempt_id);
        fs::create_dir_all(&root).unwrap();
        Self(root)
    }
    fn file(&self, path: &str) -> PathBuf {
        let path = self.0.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "fixture, never executed").unwrap();
        path
    }
    fn executable(&self, path: &str) -> PathBuf {
        let path = self.file(path);
        fs::write(&path, "#!/bin/sh\nexit 99\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        }
        path
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}
struct DefaultTool;
impl ToolAdapter for DefaultTool {
    fn id(&self) -> &'static str {
        "missing-fixture-tool"
    }
    fn recipe(&self) -> Recipe {
        Recipe::Host {
            executable: "missing-fixture-tool",
        }
    }
    fn kind(&self) -> ToolKind {
        ToolKind::Support
    }
}
#[test]
fn default_and_static_adapter_hooks_have_explicit_nonexecuting_contracts() {
    let fixture = Fixture::new();
    let project = project::ProjectConfig::default();
    let installation = crate::install::OperationContext::new(&fixture.0, &project).unwrap();
    let context = crate::runtime_inputs::Context {
        root: &fixture.0,
        invocation_dir: &fixture.0,
        project: &project,
    };
    assert!(DefaultTool.guest_assets(&fixture.0).unwrap().is_empty());
    assert!(DefaultTool.runtime_path(&fixture.0).is_none());
    assert!(DefaultTool.helper_executables().is_empty());
    assert!(!DefaultTool.host_runtime_usable(&installation).unwrap());
    assert!(DefaultTool.runtime_inputs(&context).is_err());
    DefaultTool
        .post_install(
            &installation,
            Path::new("fixture"),
            &mut |_, _| panic!("no default progress"),
            &mut Vec::new(),
        )
        .unwrap();
    fn assets(root: &Path) -> Result<Vec<GuestAsset>, crate::Error> {
        Ok(vec![GuestAsset {
            source: root.join("source"),
            target: PathBuf::from("guest-target"),
            distribution: Some(("https://fixture.invalid/archive", "fixture-hash")),
        }])
    }
    fn hook(
        _: &crate::install::OperationContext,
        _: &Path,
        progress: &mut dyn FnMut(u8, crate::install::ProgressStage),
        output: &mut dyn std::io::Write,
    ) -> crate::install::InstallResult<()> {
        progress(17, crate::install::ProgressStage::Finalizing);
        writeln!(output, "fixture postinstall").unwrap();
        Ok(())
    }
    let tool = StaticTool::new(
        "fixture",
        Recipe::Host {
            executable: "missing-fixture-tool",
        },
        ToolKind::Collector,
    )
    .with_guest_assets(assets)
    .with_host_probe(|_| Ok(true))
    .with_post_install(hook)
    .with_runtime_path()
    .with_helper_executables(&["helper"]);
    assert_eq!(tool.id(), "fixture");
    assert!(tool.kind() == ToolKind::Collector);
    assert!(tool.host_runtime_usable(&installation).unwrap());
    assert_eq!(tool.helper_executables(), ["helper"]);
    assert!(tool.runtime_path(&fixture.0).is_none());
    fs::create_dir(fixture.0.join("bin")).unwrap();
    assert_eq!(tool.runtime_path(&fixture.0), Some(fixture.0.join("bin")));
    let assets = tool.guest_assets(&fixture.0).unwrap();
    assert_eq!(assets[0].source, fixture.0.join("source"));
    assert_eq!(assets[0].target, PathBuf::from("guest-target"));
    let mut stages = Vec::new();
    let mut output = Vec::new();
    tool.post_install(
        &installation,
        Path::new("fixture"),
        &mut |percent, _| stages.push(percent),
        &mut output,
    )
    .unwrap();
    assert_eq!(stages, [17]);
    assert_eq!(output, b"fixture postinstall\n");
    assert!(
        StaticTool::new(
            "fixture",
            Recipe::Host {
                executable: "missing-fixture-tool"
            },
            ToolKind::Support
        )
        .guest_assets(&fixture.0)
        .unwrap()
        .is_empty()
    );
}
#[test]
fn runtime_path_prepends_registered_runtimes_and_respects_explicit_removal() {
    let fixture = Fixture::new();
    let tools = fixture.0.join("tools");
    let mut untouched = std::process::Command::new("never-executed");
    untouched.env("PATH", "original-path");
    apply_runtime_path(&mut untouched, &fixture.0, "tools").unwrap();
    assert_eq!(
        untouched
            .get_envs()
            .find(|(name, _)| *name == "PATH")
            .unwrap()
            .1
            .unwrap(),
        "original-path"
    );
    fs::create_dir_all(tools.join("node/bin")).unwrap();
    fs::create_dir_all(tools.join("uv/bin")).unwrap();
    let mut command = std::process::Command::new("never-executed");
    command.env("PATH", "original-path");
    apply_runtime_path(&mut command, &fixture.0, "tools").unwrap();
    let path = command
        .get_envs()
        .find(|(name, _)| *name == "PATH")
        .unwrap()
        .1
        .unwrap();
    assert_eq!(
        std::env::split_paths(path).collect::<Vec<_>>(),
        [
            tools.join("node/bin"),
            tools.join("uv/bin"),
            PathBuf::from("original-path")
        ]
    );
    command.env_remove("PATH");
    apply_runtime_path(&mut command, &fixture.0, "tools").unwrap();
    let path = command
        .get_envs()
        .find(|(name, _)| *name == "PATH")
        .unwrap()
        .1
        .unwrap();
    assert_eq!(std::env::split_paths(path).count(), 2);
    #[cfg(unix)]
    {
        let invalid = fixture.0.join("contains:colon");
        fs::create_dir_all(invalid.join("tools/node/bin")).unwrap();
        assert!(
            apply_runtime_path(&mut command, &invalid, "tools")
                .unwrap_err()
                .to_string()
                .contains("invalid runtime PATH")
        );
    }
}
#[test]
fn local_resolution_and_runtime_declarations_share_layout_and_helper_identity() {
    let fixture = Fixture::new();
    let project = project::ProjectConfig {
        tools_dir: "tools".into(),
        ..Default::default()
    };
    let context = crate::runtime_inputs::Context {
        root: &fixture.0,
        invocation_dir: &fixture.0,
        project: &project,
    };
    assert!(resolve("unknown-tool", &fixture.0, "tools").is_none());
    let processor = fixture.executable("tools/perfetto/bin/trace_processor_shell");
    let binary = fixture.file("tools/perfetto/bin/tracebox");
    #[cfg(unix)]
    assert!(resolve("perfetto", &fixture.0, "tools").is_none());
    fixture.executable("tools/perfetto/bin/tracebox");
    let expected = binary.canonicalize().unwrap();
    assert_eq!(
        resolve("perfetto", &fixture.0, "tools"),
        Some(expected.to_string_lossy().into_owned())
    );
    let dependencies = find("perfetto").unwrap().runtime_inputs(&context).unwrap();
    assert!(dependencies.contains(&crate::runtime_inputs::Dependency::program(processor)));
    assert!(dependencies.iter().any(|dependency|matches!(dependency,crate::runtime_inputs::Dependency::Program{path,..}if path==&expected)));
    fs::remove_file(binary).unwrap();
    let binary = fixture.executable("tools/perfetto/node_modules/.bin/tracebox");
    assert_eq!(
        resolve("perfetto", &fixture.0, "tools"),
        Some(
            binary
                .canonicalize()
                .unwrap()
                .to_string_lossy()
                .into_owned()
        )
    );
    fs::remove_file(binary).unwrap();
    let binary = fixture.executable("tools/perfetto/venv/bin/tracebox");
    assert_eq!(
        resolve("perfetto", &fixture.0, "tools"),
        Some(
            binary
                .canonicalize()
                .unwrap()
                .to_string_lossy()
                .into_owned()
        )
    );
    fixture.executable("tools/swebench/venv/bin/swebench");
    let dependencies = find("swebench").unwrap().runtime_inputs(&context).unwrap();
    assert!(dependencies.iter().any(|dependency|matches!(dependency,crate::runtime_inputs::Dependency::Python{path,site:true}if path==&fixture.0.join("tools/swebench/venv/bin/python"))));
    assert_eq!(names().count(), all().len());
    assert!(find("missing").is_none());
}
