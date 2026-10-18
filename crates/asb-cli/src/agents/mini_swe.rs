use super::{
    AgentArtifacts, CollectionContext, CommandContext, CommandSpec, PreparationContext,
    StaticAdapter,
};
use crate::{Error, install::Recipe};
pub(super) static ADAPTER: StaticAdapter = StaticAdapter::new(
    "mini-swe",
    "mini",
    Recipe::Python {
        package: "mini-swe-agent==2.4.6",
        dependencies: &["mcp==2.3.0"],
        executable: "mini",
    },
    command,
    prepare_adapter,
)
.with_collection(collect);
fn command(context: &CommandContext<'_>) -> CommandSpec {
    CommandSpec {
        program: "mini".into(),
        args: vec![
            "--model".into(),
            format!("anthropic/{}", context.config.model),
            "--task".into(),
            context.prompt.into(),
            "--yolo".into(),
            "--exit-immediately".into(),
            "--output".into(),
            ".asb-mini-swe.traj.json".into(),
        ],
        append_prompt: false,
    }
}
fn prepare_adapter(context: &mut PreparationContext<'_>) -> Result<(), Error> {
    if context.provider.eq_ignore_ascii_case("cli2key") {
        context
            .command
            .env("MSWEA_CONFIGURED", "true")
            .env("MSWEA_MODEL_NAME", format!("anthropic/{}", context.model))
            .env("MSWEA_SILENT_STARTUP", "1")
            .env("MSWEA_COST_TRACKING", "ignore_errors");
    }
    Ok(())
}

fn collect(context: &CollectionContext<'_>) -> AgentArtifacts {
    let trajectory = context.workspace.join(".asb-mini-swe.traj.json");
    if !trajectory.is_file() {
        return AgentArtifacts::default();
    }
    AgentArtifacts {
        token_usage: Some(crate::measurements::token_usage_from_path(&trajectory)),
        cleanup_paths: vec![trajectory],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collection_owns_the_mini_swe_trajectory_artifact() {
        let workspace =
            std::env::temp_dir().join(format!("asb-mini-swe-artifact-{}", std::process::id()));
        std::fs::create_dir_all(&workspace).expect("workspace exists");
        let trajectory = workspace.join(".asb-mini-swe.traj.json");
        std::fs::write(&trajectory, r#"{"input_tokens": 13, "output_tokens": 21}"#)
            .expect("trajectory exists");

        let artifacts = collect(&CollectionContext {
            workspace: &workspace,
        });

        assert_eq!(artifacts.cleanup_paths, vec![trajectory]);
        assert_eq!(
            artifacts
                .token_usage
                .as_ref()
                .and_then(|usage| usage.input_tokens),
            Some(13)
        );
        let _ = std::fs::remove_dir_all(workspace);
    }
}
