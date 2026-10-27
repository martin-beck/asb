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
    &[crate::endpoint::CHAT, crate::endpoint::MESSAGES_KEY],
    command,
    prepare_adapter,
)
.with_collection(collect)
.with_artifact_specs(&[super::super::ArtifactSpec {
    namespace: "mini-swe",
    filename: "trajectory.json",
    format: super::super::ArtifactFormat::Json,
}]);
fn command(context: &CommandContext<'_>) -> CommandSpec {
    CommandSpec {
        program: "mini".into(),
        args: vec![
            "--model".into(),
            format!(
                "{}/{}",
                match context.interface {
                    Some(crate::endpoint::Interface::OpenAiChat) => "openai",
                    _ => "anthropic",
                },
                context.config.model
            ),
            "--task".into(),
            context.prompt.into(),
            "--yolo".into(),
            "--exit-immediately".into(),
        ],
        prompt_delivery: super::PromptDelivery::Embedded,
    }
}
fn prepare_adapter(context: &mut PreparationContext<'_>) -> Result<(), Error> {
    context
        .command
        .arg("--output")
        .arg(context.run_dir.join("mini-swe/trajectory.json"));
    let connection = context.connection;
    let (family, prefix) = match connection.interface {
        crate::endpoint::Interface::AnthropicMessages => ("ANTHROPIC", "anthropic"),
        crate::endpoint::Interface::OpenAiChat => ("OPENAI", "openai"),
        _ => return Err(context.unsupported_endpoint("mini-swe")),
    };
    let base_url = if connection.interface == crate::endpoint::Interface::AnthropicMessages {
        connection.base_url.trim_end_matches("/v1")
    } else {
        connection.base_url.as_str()
    };
    context
        .command
        .env(
            format!("{family}_API_KEY"),
            connection.credential.as_deref().unwrap_or("asb-local"),
        )
        .env(format!("{family}_BASE_URL"), base_url);
    context
        .command
        .env("MSWEA_CONFIGURED", "true")
        .env("MSWEA_MODEL_NAME", format!("{prefix}/{}", context.model))
        .env("MSWEA_SILENT_STARTUP", "1")
        .env("MSWEA_COST_TRACKING", "ignore_errors");
    Ok(())
}

fn collect(context: &CollectionContext<'_>) -> AgentArtifacts {
    let Some(document) = context.frozen.document("trajectory.json") else {
        return AgentArtifacts::default();
    };
    AgentArtifacts {
        final_response: super::super::final_text_from_event(document),
        token_usage: Some(trajectory_usage(document)),
        steps: trajectory_steps(document),
        ..Default::default()
    }
}

fn trajectory_steps(value: &serde_json::Value) -> Option<u64> {
    // mini-SWE 2.4.6 increments this counter in query() and uses it for its
    // configured step limit. It includes failed model-call attempts, unlike
    // the number of successful response/usage records in the trajectory.
    if value.get("trajectory_format")?.as_str()? != "mini-swe-agent-1.1" {
        return None;
    }
    value.pointer("/info/model_stats/api_calls")?.as_u64()
}

fn trajectory_usage(value: &serde_json::Value) -> crate::measurements::TokenUsage {
    let fallback = || crate::usage::from_document(value);
    let Some(messages) = value.get("messages").and_then(|v| v.as_array()) else {
        return fallback();
    };
    let mut sum = crate::measurements::TokenUsage::default();
    let mut seen = false;
    let mut cache_complete = true;
    for message in messages {
        let Some(response) = message.pointer("/extra/response") else {
            continue;
        };
        let Some(value) = response.get("usage") else {
            return Default::default();
        };
        let usage = crate::usage::usage_from_json(value);
        if usage.input_tokens.is_none() || usage.output_tokens.is_none() {
            return Default::default();
        }
        cache_complete &= usage.cached_tokens.is_some();
        sum = sum.add(usage);
        seen = true;
    }
    if !seen {
        return fallback();
    }
    if !cache_complete {
        sum.cached_tokens = None;
    }
    sum.with_derived_total()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collection_uses_the_native_step_limit_counter() {
        let workspace =
            std::env::temp_dir().join(format!("asb-mini-swe-steps-{}", std::process::id()));
        std::fs::create_dir_all(&workspace).unwrap();
        let trajectory = workspace.join(".asb-mini-swe.traj.json");
        for (document, expected) in [
            (
                r#"{"trajectory_format":"mini-swe-agent-1.1","info":{"model_stats":{"api_calls":4}},"messages":[{"role":"assistant"}]}"#,
                Some(4),
            ),
            (
                r#"{"trajectory_format":"mini-swe-agent-1.1","info":{"model_stats":{"api_calls":0}}}"#,
                Some(0),
            ),
            (
                r#"{"trajectory_format":"mini-swe-agent-1.1","info":{"model_stats":{"api_calls":-1}}}"#,
                None,
            ),
            (
                r#"{"trajectory_format":"mini-swe-agent-1.1","info":{"model_stats":{"api_calls":"4"}}}"#,
                None,
            ),
            (
                r#"{"trajectory_format":"unknown","info":{"model_stats":{"api_calls":4}}}"#,
                None,
            ),
            (r#"{"api_calls":4,"messages":[{"role":"assistant"}]}"#, None),
            ("invalid", None),
        ] {
            std::fs::write(&trajectory, document).unwrap();
            let artifacts = collect(&CollectionContext {
                frozen: &serde_json::from_str(document)
                    .ok()
                    .map(|value| {
                        super::super::super::FrozenArtifacts::for_test("trajectory.json", value)
                    })
                    .unwrap_or_default(),
                run_dir: &workspace,
                interface: None,
                anthropic_input_includes_cache: false,
                mcp_servers: &[],
                mcp_tape: None,
            });
            assert_eq!(artifacts.steps, expected, "{document}");
        }
        std::fs::remove_dir_all(workspace).unwrap();
    }

    #[test]
    fn collection_owns_the_mini_swe_trajectory_artifact() {
        let workspace =
            std::env::temp_dir().join(format!("asb-mini-swe-artifact-{}", std::process::id()));
        std::fs::create_dir_all(&workspace).expect("workspace exists");
        let trajectory = workspace.join(".asb-mini-swe.traj.json");
        std::fs::write(&trajectory, r#"{"input_tokens": 13, "output_tokens": 21}"#)
            .expect("trajectory exists");

        let artifacts = collect(&CollectionContext {
            frozen: &super::super::super::FrozenArtifacts::for_test(
                "trajectory.json",
                serde_json::json!({"input_tokens":13,"output_tokens":21}),
            ),
            run_dir: &workspace,
            interface: None,
            anthropic_input_includes_cache: false,
            mcp_servers: &[],
            mcp_tape: None,
        });

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
