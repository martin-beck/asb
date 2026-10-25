//! Adapter for cli2key.
pub(in crate::providers) mod implementation;
use super::*;

fn cli2key_offers(_: &str) -> Vec<Offer> {
    offers(
        Address::Bridge,
        &[
            (Responses, Bearer),
            (Chat, Bearer),
            (Messages, ApiKey),
            (Messages, Bearer),
        ],
    )
}

adapter!(
    "cli2key",
    Some("CLI2KEY_API_KEY"),
    Some("CLI2KEY_BASE_URL"),
    Some("CLI2KEY_MODEL"),
    Some("cli2key"),
    &["CLI2KEY_BASE_URL"],
    Some("CLI2KEY_BASE_URL"),
    None,
    DataUnpaginated,
    "gpt-6.1-sol",
    cli2key_offers,
    fn runtime_inputs(
        &self,
        context: &crate::runtime_inputs::Context<'_>,
    ) -> Result<Vec<crate::runtime_inputs::Dependency>, crate::Error> {
        let mut inputs = crate::providers::credential_inputs(&self.spec());
        inputs.push(crate::runtime_inputs::Dependency::program("curl"));
        if std::env::var_os("CLI2KEY_BASE_URL").is_some() {
            return Ok(inputs);
        }
        let bridge = crate::tools::find(self.spec().bridge_tool.expect("bridge adapter tool"))
            .ok_or_else(|| crate::Error::Input("bridge tool has no runtime declaration".into()))?;
        inputs.extend(bridge.runtime_inputs(context)?);
        inputs.push(crate::runtime_inputs::Dependency::program("node"));
        let home = std::env::var_os("CODEX_HOME")
            .map(std::path::PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME").map(|home| std::path::Path::new(&home).join(".codex"))
            });
        if let Some(home) = home {
            inputs.push(crate::runtime_inputs::Dependency::file(
                home.join("auth.json"),
            ));
        }
        Ok(inputs)
    },
    fn anthropic_input_includes_cache(&self) -> bool {
        true
    },
    fn prepare(
        &self,
        command: &mut std::process::Command,
        model: &str,
        bridge: Option<(&str, &str)>,
        _tape: Option<&str>,
        _replay: bool,
    ) -> Result<(), crate::Error> {
        implementation::prepare(command, model, bridge)
    },
    fn known_models(&self) -> Option<&'static [&'static str]> {
        Some(&["gpt-6-astra", "gpt-6.1-sol", "gpt-6-luna"])
    },
    fn default_tool_support(&self) -> Option<bool> {
        Some(true)
    },
    fn start_session(
        &self,
        project_root: &std::path::Path,
        tools_dir: &str,
    ) -> Result<Option<Box<dyn crate::providers::ProviderSession>>, crate::Error> {
        let executable = crate::tools::resolve("cli2key", project_root, tools_dir);
        implementation::Bridge::connect(executable.as_deref())
            .map(|session| Some(Box::new(session) as Box<dyn crate::providers::ProviderSession>))
    },
    fn discovery_session(
        &self,
    ) -> Result<Option<Box<dyn crate::providers::ProviderSession>>, crate::Error> {
        let executable = implementation::local_executable();
        implementation::Bridge::connect(executable.as_deref())
            .map(|session| Some(Box::new(session) as Box<dyn crate::providers::ProviderSession>))
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
    fn cli2key_preparation_sets_protocol_compatible_environment() {
        let mut command = Command::new("bridge-client");
        crate::providers::preparation::apply_with_tape(
            &mut command,
            "cli2key",
            "gpt-test",
            Some(("cb_test", "http://127.0.0.1:3456/v1")),
            None,
            false,
        )
        .expect("preparation succeeds");
        let values = values(&command);
        assert_eq!(values.get("CLI2KEY_MODEL"), Some(&"gpt-test".to_owned()));
        assert_eq!(values.get("OPENAI_API_KEY"), Some(&"cb_test".to_owned()));
        assert_eq!(
            values.get("ANTHROPIC_BASE_URL"),
            Some(&"http://127.0.0.1:3456".to_owned())
        );
    }
}
