//! Adapter for openrouter.
pub(in crate::providers) mod implementation;
use super::*;

fixed_offers!(
    openrouter_offers,
    Some("OPENROUTER_BASE_URL"),
    "https://openrouter.ai/api/v1",
    "",
    false,
    &[(Chat, Bearer), (Responses, Bearer), (Messages, Bearer)]
);

adapter!(
    "openrouter",
    Some("OPENROUTER_API_KEY"),
    Some("OPENROUTER_BASE_URL"),
    Some("OPENROUTER_MODEL"),
    None,
    &["OPENROUTER_BASE_URL"],
    Some("OPENROUTER_BASE_URL"),
    Some("https://openrouter.ai/api/v1/models"),
    DataUnpaginated,
    "openai/gpt-4o-mini",
    openrouter_offers,
    fn prepare(
        &self,
        command: &mut std::process::Command,
        model: &str,
        _bridge: Option<(&str, &str)>,
        tape: Option<&str>,
        replay: bool,
    ) -> Result<(), crate::Error> {
        implementation::prepare(command, model, tape, replay)
    },
    fn public_catalog(&self, credential_present: bool) -> bool {
        !credential_present
    },
    fn supports_free(&self) -> bool {
        true
    },
    fn free_model(&self, entry: &serde_json::Value, id: &str) -> bool {
        id.ends_with(":free")
            || entry
                .get("pricing")
                .and_then(serde_json::Value::as_object)
                .is_some_and(|pricing| {
                    crate::models::is_zero_price(pricing.get("prompt"))
                        && crate::models::is_zero_price(pricing.get("completion"))
                })
    },
    fn pricing_snapshot(&self) -> Result<Option<(&'static str, serde_json::Value)>, crate::Error> {
        crate::models::openrouter_pricing_snapshot()
            .map(|snapshot| Some(("openrouter-pricing.json", snapshot)))
    }
);

#[cfg(test)]
mod tests {
    use std::process::Command;

    fn values(command: &Command) -> std::collections::HashMap<String, String> {
        command
            .get_envs()
            .filter_map(|(name, value)| {
                value.map(|value| {
                    (
                        name.to_string_lossy().into_owned(),
                        value.to_string_lossy().into_owned(),
                    )
                })
            })
            .collect()
    }

    #[test]
    fn openrouter_baseline_uses_compatible_protocol_endpoints() {
        let mut command = Command::new("bridge-client");
        crate::providers::adapters::openrouter::implementation::configure_openai_compatible(
            &mut command,
            "example/model",
            "or-test",
            "https://openrouter.ai/api/v1",
        );
        let values = values(&command);
        assert_eq!(
            values.get("OPENAI_BASE_URL"),
            Some(&"https://openrouter.ai/api/v1".to_owned())
        );
        assert_eq!(
            values.get("ANTHROPIC_BASE_URL"),
            Some(&"https://openrouter.ai/api".to_owned())
        );
    }

    #[test]
    fn replay_openrouter_does_not_need_a_live_credential() {
        let mut command = Command::new("bridge-client");
        crate::providers::preparation::apply_with_tape(
            &mut command,
            "openrouter",
            "example/model",
            None,
            Some("http://127.0.0.1:4567/v1"),
            true,
        )
        .expect("replay preparation succeeds");
        assert_eq!(
            values(&command).get("OPENROUTER_API_KEY"),
            Some(&"asb-replay".to_owned())
        );
    }
}
