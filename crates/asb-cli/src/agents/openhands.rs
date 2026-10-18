use super::{CommandContext, CommandSpec, PreparationContext, StaticAdapter};
use crate::{Error, install::Recipe};
pub(super) static ADAPTER: StaticAdapter = StaticAdapter::new(
    "openhands",
    "openhands",
    Recipe::Python {
        package: "openhands==1.16.0",
        dependencies: &[],
        executable: "openhands",
    },
    command,
    prepare_adapter,
);
fn command(context: &CommandContext<'_>) -> CommandSpec {
    CommandSpec {
        program: "openhands".into(),
        args: vec![
            "--headless".into(),
            "--json".into(),
            "--yolo".into(),
            "--override-with-envs".into(),
            "--task".into(),
            context.prompt.into(),
        ],
        append_prompt: false,
    }
}
fn prepare_adapter(context: &mut PreparationContext<'_>) -> Result<(), Error> {
    if !context.provider.eq_ignore_ascii_case("cli2key") {
        return Ok(());
    }
    let (key, url) = context
        .cli2key
        .ok_or_else(|| Error::Run("cli2key provider has no API endpoint".into()))?;
    context
        .command
        .env("LLM_API_KEY", key)
        .env("LLM_BASE_URL", url.trim_end_matches("/v1"))
        .env("LLM_MODEL", format!("anthropic/{}", context.model))
        .env("OPENHANDS_SUPPRESS_BANNER", "1");
    Ok(())
}
