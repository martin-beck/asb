use super::{StaticTool, ToolKind};
use crate::install::{InstallResult, OperationContext, ProducerDirectory, Purpose, Recipe};
pub(super) static ADAPTER: StaticTool = StaticTool::new(
    "cli2key",
    Recipe::Npm {
        package: "codex-anthropic-bridge@0.1.10",
        executable: "codex-bridge",
        manifest: None,
    },
    ToolKind::Support,
)
.with_post_install(post_install);

fn post_install(
    context: &OperationContext,
    executable: &std::path::Path,
    progress: &mut dyn FnMut(u8, crate::install::ProgressStage),
    output: &mut dyn std::io::Write,
) -> InstallResult<()> {
    progress(90, crate::install::ProgressStage::Finalizing);
    let owner =
        ProducerDirectory::open(executable.parent().ok_or_else(|| {
            crate::Error::Io("installed cli2key has no parent directory".into())
        })?)?;
    let mut scope = owner.scope(context);
    let mut step = scope.prepare(executable, Purpose::Configure, "cli2key API key refresh")?;
    step.command_mut()
        .args(["key", "refresh"])
        .stdin(std::process::Stdio::null());
    // The upstream bridge prints the freshly generated key. Keep only the
    // bounded status result; installer diagnostic logs must never retain it.
    step.suppress_diagnostics();
    let result = scope.run(step)?;
    let status = result.status;
    owner.consume(scope.finish()?)?;
    if !status.success() {
        return Err(crate::Error::Io(
            "cli2key was installed, but its local API key could not be generated".into(),
        )
        .into());
    }
    writeln!(output, "Generated a local cli2key API key for project runs")
        .map_err(crate::io_error)?;
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::tools::ToolAdapter;

    #[test]
    fn adapter_hook_reports_success_and_failure() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/cli2key-installer-fixtures")
            .join(crate::tools::runtime::unique_id());
        std::fs::create_dir_all(&root).unwrap();
        let context = OperationContext::new(
            &root.canonicalize().unwrap(),
            &crate::project::ProjectConfig::default(),
        )
        .unwrap();
        let mut updates = Vec::new();
        let mut output = Vec::new();
        ADAPTER
            .post_install(
                &context,
                &std::path::Path::new("/bin/true").canonicalize().unwrap(),
                &mut |percent, message| updates.push((percent, message.to_owned())),
                &mut output,
            )
            .unwrap();
        assert_eq!(updates, [(90, crate::install::ProgressStage::Finalizing)]);
        assert!(
            String::from_utf8(output)
                .unwrap()
                .contains("Generated a local cli2key API key")
        );
        let error = ADAPTER
            .post_install(
                &context,
                &std::path::Path::new("/bin/false").canonicalize().unwrap(),
                &mut |_, _| {},
                &mut Vec::new(),
            )
            .unwrap_err();
        assert!(error.to_string().contains("could not be generated"));
    }
}
