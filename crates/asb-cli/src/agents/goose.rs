use super::{CommandContext, CommandSpec, PreparationContext, StaticAdapter};
use crate::{Error, install::Recipe, providers};
pub(super) static ADAPTER: StaticAdapter =
    StaticAdapter::new("goose", "goose", Recipe::Goose, command, prepare_adapter);
fn command(context: &CommandContext<'_>) -> CommandSpec {
    CommandSpec {
        program: "goose".into(),
        args: vec![
            "run".into(),
            "--no-session".into(),
            "--max-turns".into(),
            "1".into(),
            "--output-format".into(),
            "json".into(),
            "-t".into(),
            context.prompt.into(),
        ],
        append_prompt: false,
    }
}
fn prepare_adapter(context: &mut PreparationContext<'_>) -> Result<(), Error> {
    if context.provider.eq_ignore_ascii_case("cli2key") {
        if let Some((key, url)) = context.cli2key {
            context
                .command
                .env("GOOSE_PROVIDER", "anthropic")
                .env("GOOSE_MODEL", context.model)
                .env("ANTHROPIC_HOST", url.trim_end_matches("/v1"))
                .env("ANTHROPIC_API_KEY", key)
                .env("ANTHROPIC_AUTH_TOKEN", key);
        }
    } else if context.provider.eq_ignore_ascii_case("openrouter") {
        let key = providers::credential("openrouter").ok_or_else(|| {
            Error::Run(
                "openrouter provider requires OPENROUTER_API_KEY or ~/.api_key_openrouter".into(),
            )
        })?;
        let base_url = std::env::var("OPENROUTER_BASE_URL")
            .unwrap_or_else(|_| "https://openrouter.ai/api/v1".into());
        context
            .command
            .env("GOOSE_PROVIDER", "openai")
            .env("GOOSE_MODEL", context.model)
            .env("OPENAI_HOST", base_url.trim_end_matches("/v1"))
            .env("OPENAI_BASE_PATH", "/v1/chat/completions")
            .env("OPENAI_API_KEY", key);
    }
    Ok(())
}
