use super::{CommandContext, CommandSpec, PreparationContext, StaticAdapter};
use crate::{Error, install::Recipe};
pub(super) static ADAPTER: StaticAdapter = StaticAdapter::new(
    "workbuddy",
    "codebuddy",
    Recipe::Npm {
        package: "@tencent-ai/codebuddy-code@2.167.0",
        executable: "codebuddy",
        manifest: None,
    },
    &[crate::endpoint::RESPONSES],
    command,
    prepare_adapter,
)
.with_replay_credential_vars(&["CODEBUDDY_API_KEY", "CODEBUDDY_AUTH_TOKEN"])
.with_state_environment_vars(&["CODEBUDDY_CONFIG_DIR"])
.with_output_usage(output_usage)
.with_step_count(step_count);

fn step_count(stdout: &str, _stderr: &str) -> Option<u64> {
    // Use CodeBuddy's own active-agent history counter from its final result.
    super::super::final_result_counter(stdout, "num_turns")
}

fn output_usage(stdout: &str, stderr: &str, _: bool) -> crate::measurements::TokenUsage {
    fn result(value: &serde_json::Value) -> Option<crate::measurements::TokenUsage> {
        match value {
            serde_json::Value::Array(events) => events.iter().rev().find_map(result),
            serde_json::Value::Object(event)
                if event.get("type").and_then(|v| v.as_str()) == Some("result") =>
            {
                event.get("usage").map(super::super::usage_fields)
            }
            _ => None,
        }
    }
    serde_json::from_str::<serde_json::Value>(stdout)
        .ok()
        .and_then(|v| result(&v))
        .unwrap_or_else(|| crate::usage::usage_from_text(&format!("{stdout}\n{stderr}")))
        .with_derived_total()
}
fn command(context: &CommandContext<'_>) -> CommandSpec {
    CommandSpec {
        program: "codebuddy".into(),
        args: vec![
            "--print".into(),
            "--output-format".into(),
            "json".into(),
            "--model".into(),
            context.config.model.clone(),
            "-y".into(),
        ],
        append_prompt: true,
    }
}
fn prepare_adapter(context: &mut PreparationContext<'_>) -> Result<(), Error> {
    let connection = context.connection;
    let key = connection.credential.as_deref().unwrap_or("asb-local");
    let base_url = &connection.base_url;
    let directory = context.runtime_dir.join("workbuddy/config");
    configure(&directory, context.model, base_url)?;
    context
        .command
        .env("CODEBUDDY_API_KEY", key)
        .env("CODEBUDDY_AUTH_TOKEN", key)
        .env("CODEBUDDY_BASE_URL", base_url)
        .env("CODEBUDDY_MODEL", context.model)
        .env("CODEBUDDY_CONFIG_DIR", &directory)
        .env("CODEBUDDY_API_KEY_HELPER_DISABLED", "1");
    Ok(())
}

fn configure(directory: &std::path::Path, model: &str, base_url: &str) -> Result<(), Error> {
    std::fs::create_dir_all(directory).map_err(|error| {
        Error::Run(format!(
            "cannot create WorkBuddy provider configuration directory: {error}"
        ))
    })?;
    let configuration = serde_json::json!([{
        "id": model, "name": model, "url": base_url, "api": "openai-responses"
    }]);
    let content = serde_json::to_vec_pretty(&configuration).map_err(|error| {
        Error::Run(format!(
            "cannot render WorkBuddy provider configuration: {error}"
        ))
    })?;
    std::fs::write(directory.join("models.json"), content).map_err(|error| {
        Error::Run(format!(
            "cannot write WorkBuddy provider configuration: {error}"
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::AgentAdapter;

    #[test]
    fn adapter_reports_final_native_turns_without_counting_snapshots() {
        assert_eq!(
            ADAPTER.step_count(
                r#"[{"type":"result","num_turns":3},{"type":"result","num_turns":3}]"#,
                ""
            ),
            Some(3)
        );
        assert_eq!(
            ADAPTER.step_count(r#"{"type":"result","subtype":"error","num_turns":2}"#, ""),
            Some(2)
        );
        assert_eq!(
            ADAPTER.step_count(r#"{"type":"result","num_turns":-1}"#, ""),
            None
        );
        assert_eq!(ADAPTER.step_count(r#"{"num_turns":3}"#, ""), None);
    }

    #[test]
    fn provider_configuration_is_owned_by_the_workbuddy_adapter() {
        let directory =
            std::env::temp_dir().join(format!("asb-workbuddy-config-{}", std::process::id()));
        configure(&directory, "test-model", "http://127.0.0.1:3456/v1")
            .expect("configuration succeeds");
        let text =
            std::fs::read_to_string(directory.join("models.json")).expect("configuration exists");
        assert!(text.contains("test-model"));
        let _ = std::fs::remove_dir_all(directory);
    }
}
