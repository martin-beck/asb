//! Provider-specific adapter registry. Each module owns one provider's identity,
//! discovery details, and offline UI model suggestion.
use super::{ModelKind, ModelSpec, ProviderSpec};
use crate::endpoint::{Address, Authentication, Interface, Offer};

type EndpointFactory = fn(&str) -> Vec<Offer>;
fn offers(address: Address, capabilities: &[(Interface, Authentication)]) -> Vec<Offer> {
    capabilities
        .iter()
        .map(|&(interface, authentication)| Offer {
            interface,
            authentication,
            address: address.clone(),
        })
        .collect()
}

macro_rules! fixed_offers {
    ($function:ident, $env:expr, $url:literal, $suffix:literal, $infer_http:expr, $capabilities:expr) => {
        fn $function(_: &str) -> Vec<Offer> {
            offers(
                Address::Configured {
                    environment: $env,
                    default: $url,
                    suffix: $suffix,
                    infer_http: $infer_http,
                },
                $capabilities,
            )
        }
    };
}

use Authentication::{ApiKey, Bearer, None as NoAuth};
use Interface::{
    AnthropicMessages as Messages, Gemini, OpenAiChat as Chat, OpenAiResponses as Responses,
};
pub(super) trait ProviderAdapter: Sync {
    fn runtime_inputs(
        &self,
        _context: &crate::runtime_inputs::Context<'_>,
    ) -> Result<Vec<crate::runtime_inputs::Dependency>, crate::Error> {
        let mut inputs = super::credential_inputs(&self.spec());
        inputs.push(crate::runtime_inputs::Dependency::program("curl"));
        Ok(inputs)
    }
    /// Whether Anthropic-compatible input usage already includes cached input.
    fn anthropic_input_includes_cache(&self) -> bool {
        false
    }
    fn tape_environment(&self) -> &'static [&'static str];
    fn tape_root_environment(&self) -> &'static [&'static str] {
        &[]
    }
    fn spec(&self) -> ProviderSpec;
    fn model_spec(&self) -> ModelSpec;
    fn default_model(&self) -> &'static str;
    fn offered_endpoints(&self, model: &str) -> Vec<Offer>;
    fn discover_endpoints(&self, model: &str) -> (Vec<Offer>, bool, Option<bool>) {
        (self.offered_endpoints(model), false, None)
    }
    fn replay_endpoints(&self, model: &str, _recorded: Option<Interface>) -> Vec<Offer> {
        self.offered_endpoints(model)
    }
    fn prepare(
        &self,
        _command: &mut std::process::Command,
        _model: &str,
        _bridge: Option<(&str, &str)>,
        _tape: Option<&str>,
        _replay: bool,
    ) -> Result<(), crate::Error> {
        Ok(())
    }
    fn public_catalog(&self, _credential_present: bool) -> bool {
        false
    }
    fn supports_free(&self) -> bool {
        false
    }
    fn free_model(&self, _entry: &serde_json::Value, _id: &str) -> bool {
        false
    }
    fn known_models(&self) -> Option<&'static [&'static str]> {
        None
    }
    fn default_tool_support(&self) -> Option<bool> {
        None
    }
    fn start_session(
        &self,
        _project_root: &std::path::Path,
        _tools_dir: &str,
    ) -> Result<Option<Box<dyn super::ProviderSession>>, crate::Error> {
        Ok(None)
    }
    fn discovery_session(&self) -> Result<Option<Box<dyn super::ProviderSession>>, crate::Error> {
        Ok(None)
    }
    fn tool_support(&self, _entry: &serde_json::Value) -> Option<bool> {
        None
    }
    fn enrich_catalog(&self, body: Vec<u8>) -> Result<Vec<u8>, crate::Error> {
        Ok(body)
    }
    fn catalog_base(&self, _selected: Option<&str>, fallback: Option<&str>) -> Option<String> {
        fallback.map(str::to_owned)
    }
    fn catalog_normalize(&self, base: String) -> String {
        base
    }
    fn catalog_suffix(&self, _base: &str) -> &'static str {
        "/models"
    }
    fn catalog_url_error(&self) -> Option<&'static str> {
        None
    }
    fn catalog_headers(&self, key: &str) -> Vec<String> {
        vec![format!("Authorization: Bearer {key}")]
    }
    /// Provider-owned metadata captured before a fresh sweep starts.
    fn pricing_snapshot(&self) -> Result<Option<(&'static str, serde_json::Value)>, crate::Error> {
        Ok(None)
    }
    fn pricing_filename(&self) -> Option<&'static str> {
        None
    }
    fn validate_pricing(&self, _snapshot: &serde_json::Value) -> Result<(), crate::Error> {
        Ok(())
    }
    fn estimate_cost(
        &self,
        _snapshot: &serde_json::Value,
        _model: &str,
        _metrics: &std::collections::BTreeMap<String, f64>,
    ) -> Option<f64> {
        None
    }
}

macro_rules! adapter {
    ($id:literal, $key:expr, $endpoint:expr, $model_env:expr, $bridge:expr, $tape_envs:expr,
     $model_endpoint:expr, $model_url:expr, $kind:ident, $suggestion:literal, $offers:expr $(, $extra:item)*) => {
            pub(in crate::providers) struct Adapter;
            pub(in crate::providers) static ADAPTER: Adapter = Adapter;
            impl ProviderAdapter for Adapter {
                fn tape_environment(&self) -> &'static [&'static str] {
                    $tape_envs
                }
                fn spec(&self) -> ProviderSpec {
                    ProviderSpec {
                        name: $id,
                        credential: $key,
                        endpoint: $endpoint,
                        model_env: $model_env,
                        tape_endpoint_envs: tape_endpoint_envs(),
                        bridge_tool: $bridge,
                    }
                }
                fn model_spec(&self) -> ModelSpec {
                    ModelSpec {
                        name: $id,
                        credential: $key,
                        endpoint: $model_endpoint,
                        default_url: $model_url,
                        kind: ModelKind::$kind,
                    }
                }
                fn default_model(&self) -> &'static str {
                    $suggestion
                }
                fn offered_endpoints(&self, model: &str) -> Vec<Offer> {
                    let factory: EndpointFactory = $offers;
                    factory(model)
                }
                $($extra)*
            }
    };
}

mod anthropic;
pub(super) mod cli2key;
mod deepseek;
pub(super) mod fireworks;
mod gemini;
mod groq;
mod local;
mod mistral;
pub(super) mod ollama;
mod openai;
pub(super) mod opencode_zen;
pub(super) mod openrouter;
mod together;
mod xai;

pub(super) static ADAPTERS: &[&dyn ProviderAdapter] = &[
    &openai::ADAPTER,
    &openrouter::ADAPTER,
    &opencode_zen::ADAPTER,
    &anthropic::ADAPTER,
    &gemini::ADAPTER,
    &ollama::ADAPTER,
    &groq::ADAPTER,
    &mistral::ADAPTER,
    &deepseek::ADAPTER,
    &xai::ADAPTER,
    &together::ADAPTER,
    &fireworks::ADAPTER,
    &cli2key::ADAPTER,
];

pub(super) fn find(name: &str) -> Option<&'static dyn ProviderAdapter> {
    ADAPTERS
        .iter()
        .copied()
        .find(|adapter| adapter.spec().name.eq_ignore_ascii_case(name))
}

/// Route every known endpoint alias through replay, including aliases inherited
/// from the parent environment for a different provider.
pub(super) fn tape_endpoint_envs() -> &'static [&'static str] {
    static ENVS: std::sync::OnceLock<Vec<&'static str>> = std::sync::OnceLock::new();
    ENVS.get_or_init(|| {
        ADAPTERS
            .iter()
            .flat_map(|adapter| adapter.tape_environment().iter().copied())
            .collect()
    })
}

pub(super) fn tape_root_endpoint_envs() -> &'static [&'static str] {
    static ENVS: std::sync::OnceLock<Vec<&'static str>> = std::sync::OnceLock::new();
    ENVS.get_or_init(|| {
        ADAPTERS
            .iter()
            .flat_map(|adapter| adapter.tape_root_environment().iter().copied())
            .collect()
    })
}

pub(super) fn fallback_spec(name: &str) -> Option<ProviderSpec> {
    local::spec(name)
}

/// Map a negotiated protocol to the provider label expected by an agent CLI.
/// The one provider-local alias remains inside the provider adapter boundary.
pub(super) fn agent_provider_name(
    provider: &str,
    interface: Option<crate::endpoint::Interface>,
) -> &'static str {
    if provider == "cli2key" {
        "asb-cli2key"
    } else {
        match interface {
            Some(crate::endpoint::Interface::AnthropicMessages) => "anthropic",
            _ => "openai-api",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A lightweight regression guard for direct provider-ID dispatch. It does
    // not replace review: indirect comparisons and generated code need review.
    fn has_provider_id_dispatch(source: &str, names: &[&str]) -> bool {
        let code: String = source
            .split("#[cfg(test)]")
            .next()
            .unwrap_or("")
            .chars()
            .filter(|character| !character.is_whitespace())
            .collect();
        names.iter().any(|name| {
            let literal = format!("\"{name}\"");
            let direct = [
                format!("provider=={literal}"),
                format!("{literal}==provider"),
                format!("provider.eq_ignore_ascii_case({literal})"),
                format!("provider.as_str()=={literal}"),
            ]
            .iter()
            .any(|pattern| code.contains(pattern));
            let matched = [
                "matchprovider{",
                "matchprovider.as_str(){",
                "matchconfig.provider.as_str(){",
            ]
            .iter()
            .filter_map(|pattern| code.split_once(pattern).map(|(_, tail)| tail))
            .any(|tail| {
                let mut depth = 1usize;
                let mut end = tail.len();
                for (index, byte) in tail.bytes().enumerate() {
                    match byte {
                        b'{' => depth += 1,
                        b'}' => {
                            depth -= 1;
                            if depth == 0 {
                                end = index;
                                break;
                            }
                        }
                        _ => {}
                    }
                }
                let arms = &tail[..end];
                arms.contains(&format!("{literal}=>")) || arms.contains(&format!("{literal}|"))
            });
            direct || matched
        })
    }

    #[test]
    fn direct_provider_id_branching_stays_in_provider_adapters() {
        fn visit(path: &std::path::Path, names: &[&str]) {
            for entry in std::fs::read_dir(path).expect("source directory is readable") {
                let entry = entry.expect("source entry is readable");
                let path = entry.path();
                if path.is_dir() {
                    if !(entry.file_name() == "adapters"
                        && path.parent().and_then(|parent| parent.file_name())
                            == Some(std::ffi::OsStr::new("providers")))
                    {
                        visit(&path, names);
                    }
                } else if path.extension().is_some_and(|extension| extension == "rs") {
                    let source = std::fs::read_to_string(&path).expect("source file is readable");
                    assert!(
                        !has_provider_id_dispatch(&source, names),
                        "provider-specific dispatch outside adapters: {}",
                        path.display()
                    );
                }
            }
        }
        let names: Vec<_> = ADAPTERS.iter().map(|adapter| adapter.spec().name).collect();
        assert!(has_provider_id_dispatch(
            "if provider == \"openrouter\" {}",
            &names
        ));
        assert!(has_provider_id_dispatch(
            "match provider { \"ollama\" => {} }",
            &names
        ));
        visit(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
            &names,
        );
    }

    #[test]
    fn each_provider_is_uniquely_registered_and_coherent() {
        let mut names = std::collections::HashSet::new();
        for adapter in ADAPTERS {
            let spec = adapter.spec();
            assert!(
                names.insert(spec.name.to_ascii_lowercase()),
                "duplicate provider {}",
                spec.name
            );
            assert_eq!(adapter.model_spec().name, spec.name);
            assert!(
                !adapter.default_model().is_empty(),
                "missing suggestion for {}",
                spec.name
            );
            assert!(
                find(&spec.name.to_ascii_uppercase()).is_some(),
                "case-insensitive lookup failed for {}",
                spec.name
            );
            assert!(
                !adapter
                    .offered_endpoints(adapter.default_model())
                    .is_empty(),
                "no endpoint offer for {}",
                spec.name
            );
        }
        assert_eq!(names.len(), crate::providers::names().len());
    }

    #[test]
    fn replay_redirects_all_registered_endpoint_aliases_without_duplicates() {
        let aliases = tape_endpoint_envs();
        let unique: std::collections::HashSet<_> = aliases.iter().collect();
        assert_eq!(aliases.len(), unique.len());
        for adapter in ADAPTERS {
            for alias in adapter.tape_environment() {
                assert!(aliases.contains(alias));
            }
        }
    }
}
