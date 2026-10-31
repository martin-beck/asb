//! Small provider registry for environment-driven integrations.

mod adapters;
mod preparation;
pub(crate) use preparation::apply_with_tape;

use crate::Error;
use crate::endpoint::Interface;
use std::fs;
use std::path::{Path, PathBuf};

const MAX_KEY_FILE_BYTES: u64 = 4096;

/// One provider's conventional environment names.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProviderSpec {
    /// Stable name accepted by `asb setup --provider`.
    pub name: &'static str,
    /// Environment variable holding the provider credential, when applicable.
    pub credential: Option<&'static str>,
    /// Environment variable commonly used to override the provider endpoint.
    pub endpoint: Option<&'static str>,
    /// Provider-specific model selection environment variable.
    pub model_env: Option<&'static str>,
    /// Endpoint variables that must route through the local tape proxy.
    pub tape_endpoint_envs: &'static [&'static str],
    /// Project-local bridge tool used by this provider, when required.
    pub bridge_tool: Option<&'static str>,
}

/// A provider-owned local process or connection whose lifetime spans the run.
pub(crate) trait ProviderSession {
    fn api_key(&self) -> &str;
    fn base_url(&self) -> &str;
    /// Explicit normal teardown; Drop remains a fallback for unwinding.
    fn finish(&mut self) -> Result<(), crate::Error> {
        Ok(())
    }
}

/// Consume normal session custody exactly once, including a failed finish.
pub(crate) fn finish_session(
    session: &mut Option<Box<dyn ProviderSession>>,
) -> Result<(), crate::Error> {
    session
        .take()
        .map_or(Ok(()), |mut session| session.finish())
}

/// Settle an early operation, retaining its primary Input/Io/Run variant.
/// Usage's static message becomes Run when a dynamic cleanup suffix is needed.
pub(crate) fn finish_result<T>(
    result: Result<T, crate::Error>,
    session: &mut Option<Box<dyn ProviderSession>>,
) -> Result<T, crate::Error> {
    match (result, finish_session(session)) {
        (result, Ok(())) => result,
        (Ok(_), Err(cleanup)) => Err(cleanup),
        (Err(primary), Err(cleanup)) => {
            let append = |reason: String| format!("{reason}; provider cleanup failed: {cleanup}");
            Err(match primary {
                crate::Error::Input(reason) => crate::Error::Input(append(reason)),
                crate::Error::Io(reason) => crate::Error::Io(append(reason)),
                crate::Error::Run(reason) => crate::Error::Run(append(reason)),
                crate::Error::Usage(reason) => crate::Error::Run(append(reason.into())),
            })
        }
    }
}

/// Response shape used by a provider's model-list endpoint.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelKind {
    DataUnpaginated,
    ModelsPageTokenApiKey,
    DataCursor,
    ModelsUnpaginated,
    ArrayUnpaginated,
    ModelsPageTokenBearer,
}

/// Live model discovery details for a supported provider.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ModelSpec {
    pub name: &'static str,
    pub credential: Option<&'static str>,
    pub endpoint: Option<&'static str>,
    pub default_url: Option<&'static str>,
    pub kind: ModelKind,
}

impl ModelSpec {
    /// Resolve an endpoint override or the provider's documented default.
    pub fn url(self) -> Option<String> {
        self.url_for_model(None)
    }

    /// Resolve the catalog endpoint for an optionally selected model.
    pub fn url_for_model(self, selected: Option<&str>) -> Option<String> {
        let adapter = adapters::find(self.name);
        let base = self
            .endpoint
            .and_then(std::env::var_os)
            .map(|value| value.to_string_lossy().into_owned())
            .or_else(|| match adapter {
                Some(adapter) => adapter.catalog_base(selected, self.default_url),
                None => self.default_url.map(str::to_owned),
            })?;
        let base = adapter.map_or(base.clone(), |adapter| adapter.catalog_normalize(base));
        let trimmed = base.trim_end_matches('/');
        if trimmed.ends_with("/models") || trimmed.ends_with("/tags") {
            return Some(trimmed.to_owned());
        }
        let suffix = adapter.map_or("/models", |adapter| adapter.catalog_suffix(trimmed));
        Some(format!("{trimmed}{suffix}"))
    }
}

/// Return a known provider or a generic custom provider specification.
pub fn spec(name: &str) -> ProviderSpec {
    adapters::find(name)
        .map(|adapter| adapter.spec())
        .or_else(|| adapters::fallback_spec(name))
        .unwrap_or(ProviderSpec {
            name: "custom",
            credential: None,
            endpoint: None,
            model_env: None,
            tape_endpoint_envs: adapters::tape_endpoint_envs(),
            bridge_tool: None,
        })
}

/// Names of providers for which ASB can attempt live model discovery.
pub fn names() -> &'static [&'static str] {
    static NAMES: std::sync::OnceLock<Vec<&'static str>> = std::sync::OnceLock::new();
    NAMES.get_or_init(|| {
        adapters::ADAPTERS
            .iter()
            .map(|adapter| adapter.spec().name)
            .collect()
    })
}

/// Providers whose model responses have an unambiguous free-of-charge meaning.
pub fn free_names() -> &'static [&'static str] {
    static NAMES: std::sync::OnceLock<Vec<&'static str>> = std::sync::OnceLock::new();
    NAMES.get_or_init(|| {
        adapters::ADAPTERS
            .iter()
            .filter(|adapter| adapter.supports_free())
            .map(|adapter| adapter.spec().name)
            .collect()
    })
}

/// Return whether `--free` can classify this provider's model catalog safely.
pub fn supports_free_models(name: &str) -> bool {
    free_names()
        .iter()
        .any(|candidate| candidate.eq_ignore_ascii_case(name))
}

/// Offline UI suggestions only; never use this stale snapshot to validate runs.
pub fn known_models(provider: &str) -> Option<&'static [&'static str]> {
    adapters::find(provider).and_then(|adapter| adapter.known_models())
}

pub(crate) fn anthropic_input_includes_cache(provider: &str) -> bool {
    adapters::find(provider).is_some_and(|adapter| adapter.anthropic_input_includes_cache())
}

pub(crate) fn public_catalog(provider: &str, credential_present: bool) -> bool {
    adapters::find(provider).is_some_and(|adapter| adapter.public_catalog(credential_present))
}

pub(crate) fn free_model(provider: &str, entry: &serde_json::Value, model: &str) -> bool {
    adapters::find(provider).is_some_and(|adapter| adapter.free_model(entry, model))
}

pub(crate) fn default_tool_support(provider: &str) -> Option<bool> {
    adapters::find(provider).and_then(|adapter| adapter.default_tool_support())
}

pub(crate) fn catalog_tool_support(provider: &str, entry: &serde_json::Value) -> Option<bool> {
    adapters::find(provider).and_then(|adapter| adapter.tool_support(entry))
}

pub(crate) fn enrich_catalog(provider: &str, body: Vec<u8>) -> Result<Vec<u8>, crate::Error> {
    match adapters::find(provider) {
        Some(adapter) => adapter.enrich_catalog(body),
        None => Ok(body),
    }
}

pub(crate) fn catalog_url_error(provider: &str) -> Option<&'static str> {
    adapters::find(provider).and_then(|adapter| adapter.catalog_url_error())
}

pub(crate) fn catalog_headers(provider: &str, key: &str) -> Vec<String> {
    adapters::find(provider).map_or_else(
        || vec![format!("Authorization: Bearer {key}")],
        |adapter| adapter.catalog_headers(key),
    )
}

pub(crate) fn discovery_session(
    provider: &str,
) -> Result<Option<Box<dyn ProviderSession>>, crate::Error> {
    match adapters::find(provider) {
        Some(adapter) => adapter.discovery_session(),
        None => Ok(None),
    }
}

/// Validate a user-facing provider or agent identifier.
/// Return the endpoint and credential information for live discovery.
pub fn model_spec(name: &str) -> Option<ModelSpec> {
    adapters::find(name).map(|adapter| adapter.model_spec())
}

pub(crate) fn default_model(name: &str) -> &'static str {
    adapters::find(name).map_or("test-model", |adapter| adapter.default_model())
}

/// Resolve a provider credential from the environment or the user's local key file.
///
/// Environment variables take precedence. Key files are bounded and never printed;
/// they are intended only as a convenience for local development.
pub fn credential(name: &str) -> Option<String> {
    let spec = model_spec(name)?;
    let variable = spec.credential?;
    if let Some(value) = std::env::var_os(variable) {
        let value = value.to_string_lossy().trim().to_owned();
        if !value.is_empty() {
            return Some(value);
        }
    }
    let home = std::env::var_os("HOME")?;
    read_key_file(Path::new(&home).join(format!(".api_key_{}", spec.name)))
}

/// Ask a provider adapter for its current pricing metadata, if it publishes one.
pub(crate) fn pricing_snapshot(
    provider: &str,
) -> Result<Option<(&'static str, serde_json::Value)>, Error> {
    adapters::find(provider).map_or(Ok(None), |adapter| adapter.pricing_snapshot())
}

fn credential_inputs(spec: &ProviderSpec) -> Vec<crate::runtime_inputs::Dependency> {
    let Some(variable) = spec.credential else {
        return Vec::new();
    };
    if std::env::var_os(variable).is_some_and(|value| !value.to_string_lossy().trim().is_empty()) {
        return Vec::new();
    }
    std::env::var_os("HOME")
        .map(|home| {
            vec![crate::runtime_inputs::Dependency::file(
                Path::new(&home).join(format!(".api_key_{}", spec.name)),
            )]
        })
        .unwrap_or_default()
}

pub(crate) fn runtime_inputs(
    name: &str,
    context: &crate::runtime_inputs::Context<'_>,
) -> Result<Vec<crate::runtime_inputs::Dependency>, Error> {
    adapters::find(name).map_or_else(|| Ok(Vec::new()), |adapter| adapter.runtime_inputs(context))
}

/// Interfaces offered by the provider for this model, in provider preference
/// order. A model whose endpoint family is not yet known must not be guessed.
pub(crate) fn offered_endpoints(provider: &str, model: &str) -> Vec<crate::endpoint::Offer> {
    adapters::find(provider).map_or_else(Vec::new, |adapter| adapter.offered_endpoints(model))
}

/// Return the provider label an agent CLI should receive for a negotiated
/// connection. Provider-specific aliases stay in the provider layer so agent
/// adapters remain independent of provider identifiers.
pub(crate) fn agent_provider_name(
    provider: &str,
    interface: Option<crate::endpoint::Interface>,
) -> &'static str {
    adapters::agent_provider_name(provider, interface)
}

/// Refresh model-specific endpoint metadata where the provider publishes it.
/// Other providers retain their documented interface registry and explicit URL
/// overrides; a model list alone does not establish wire-protocol support.
pub(crate) fn discover_endpoints(
    provider: &str,
    model: &str,
) -> (Vec<crate::endpoint::Offer>, bool, Option<bool>) {
    adapters::find(provider).map_or_else(
        || (Vec::new(), false, None),
        |adapter| adapter.discover_endpoints(model),
    )
}

/// Replay uses the cassette's recorded wire interface, not a live catalog.
pub(crate) fn replay_endpoints(
    provider: &str,
    model: &str,
    recorded: Option<Interface>,
) -> Vec<crate::endpoint::Offer> {
    adapters::find(provider).map_or_else(Vec::new, |adapter| {
        adapter.replay_endpoints(model, recorded)
    })
}

pub(crate) fn endpoint_connection(
    provider: &str,
    offer: crate::endpoint::Offer,
    bridge: Option<(&str, &str)>,
    tape: Option<(crate::tape::Mode, &str)>,
) -> Result<crate::endpoint::Connection, crate::Error> {
    let metadata = model_spec(provider).ok_or_else(|| {
        crate::Error::Run(format!(
            "provider `{provider}` has no configured API endpoint"
        ))
    })?;
    let endpoint = if let Some((_, url)) = tape {
        url.to_owned()
    } else {
        endpoint_address(provider, offer.address, bridge.map(|(_, url)| url))?
    };
    let credential = if tape.is_some_and(|(mode, _)| mode == crate::tape::Mode::Replay) {
        Some("asb-replay".to_owned())
    } else if let Some((key, _)) = bridge {
        Some(key.to_owned())
    } else if let Some(variable) = metadata.credential {
        Some(credential(provider).ok_or_else(|| crate::Error::Run(format!(
            "provider `{provider}` requires a credential; set {variable} before starting this run"
        )))?)
    } else {
        None
    };
    Ok(crate::endpoint::Connection {
        interface: offer.interface,
        base_url: endpoint,
        authentication: offer.authentication,
        credential,
    })
}

fn endpoint_address(
    provider: &str,
    address: crate::endpoint::Address,
    bridge: Option<&str>,
) -> Result<String, crate::Error> {
    match address {
        crate::endpoint::Address::Bridge => bridge.map(str::to_owned).ok_or_else(|| {
            crate::Error::Run(format!(
                "provider `{provider}` requires its local bridge to be started"
            ))
        }),
        crate::endpoint::Address::Configured {
            environment,
            default,
            suffix,
            infer_http,
        } => {
            let mut base = environment
                .and_then(std::env::var_os)
                .map(|v| v.to_string_lossy().into_owned())
                .unwrap_or_else(|| default.to_owned());
            if infer_http && !base.contains("://") {
                base = format!("http://{base}");
            }
            let base = base.trim_end_matches('/');
            Ok(if suffix.is_empty() || base.ends_with(suffix) {
                base.to_owned()
            } else {
                format!("{base}{suffix}")
            })
        }
        crate::endpoint::Address::Discovered {
            environment,
            default,
        } => Ok(environment
            .and_then(std::env::var_os)
            .map(|v| v.to_string_lossy().trim_end_matches('/').to_owned())
            .unwrap_or(default)),
    }
}

/// The proxy receives /v1/... paths; retain the selected API's path prefix.
pub(crate) fn selected_tape_upstream(
    provider: &str,
    offer: crate::endpoint::Offer,
    bridge: Option<&str>,
) -> Result<String, crate::Error> {
    endpoint_address(provider, offer.address, bridge).map(|url| strip_api_version(&url))
}

fn read_key_file(path: PathBuf) -> Option<String> {
    let metadata = fs::metadata(path.as_path()).ok()?;
    if !metadata.is_file() || metadata.len() > MAX_KEY_FILE_BYTES {
        return None;
    }
    let value = fs::read_to_string(path).ok()?.trim().to_owned();
    (!value.is_empty() && !value.contains(['\r', '\n'])).then_some(value)
}

/// Add provider-neutral and provider-specific selection variables to a child.
///
/// The child inherits the parent environment. During replay, known provider
/// credentials are removed and only the selected provider gets a placeholder.
pub fn configure(command: &mut std::process::Command, provider: &str, model: &str, replay: bool) {
    let selected = spec(provider);
    if replay {
        command.env_remove("ASB_AGENT_API_KEY");
        for variable in adapters::ADAPTERS
            .iter()
            .filter_map(|adapter| adapter.spec().credential)
        {
            command.env_remove(variable);
        }
        if let Some(variable) = selected.credential {
            command.env(variable, "asb-replay");
        }
    } else if let Some(variable) = selected.credential
        && let Some(value) = credential(provider)
    {
        command.env(variable, value);
    }
    command
        .env("ASB_PROVIDER", provider)
        .env("ASB_MODEL", model);
    if let Some(name) = selected.credential {
        command.env("ASB_PROVIDER_CREDENTIAL_ENV", name);
    }
    if let Some(name) = selected.endpoint {
        command.env("ASB_PROVIDER_ENDPOINT_ENV", name);
    }
    if let Some(name) = selected.model_env {
        command.env(name, model);
    }
}

/// Start a provider-owned local bridge when that provider requires one.
///
/// The runner deliberately does not know bridge executable names or tool IDs;
/// that relationship is owned by the provider integration.
pub(crate) fn start_local_bridge(
    provider: &str,
    project_root: &Path,
    tools_dir: &str,
    replay: bool,
) -> Result<Option<Box<dyn ProviderSession>>, crate::Error> {
    if replay {
        return Ok(None);
    }
    match adapters::find(provider) {
        Some(adapter) => adapter.start_session(project_root, tools_dir),
        None => Ok(None),
    }
}

/// Return local-bridge credentials only for the provider that owns the bridge.
pub(crate) fn local_bridge_credentials<'a>(
    provider: &str,
    bridge: Option<&'a dyn ProviderSession>,
    endpoint: Option<&'a str>,
) -> Option<(&'a str, &'a str)> {
    spec(provider).bridge_tool?;
    Some((
        bridge.map_or("asb-replay", ProviderSession::api_key),
        endpoint.unwrap_or("http://127.0.0.1:0/v1"),
    ))
}

/// Return the endpoint of a provider-owned bridge without exposing its tool
/// implementation to the runner.
pub(crate) fn local_bridge_endpoint<'a>(
    provider: &str,
    bridge: Option<&'a dyn ProviderSession>,
) -> Option<&'a str> {
    spec(provider)
        .bridge_tool
        .and_then(|_| bridge.map(ProviderSession::base_url))
}

/// Configure a child to route this provider through the tape proxy.
pub(crate) fn configure_tape(
    command: &mut std::process::Command,
    provider: &str,
    url: &str,
    mode: crate::tape::Mode,
) {
    let selected = spec(provider);
    let root_url = url.trim_end_matches("/v1");
    for name in selected.tape_endpoint_envs {
        command.env(name, url);
    }
    for name in adapters::tape_root_endpoint_envs() {
        command.env(name, root_url);
    }
    command
        .env("LLM_BASE_URL", root_url)
        .env("ASB_PROVIDER_BASE_URL", url)
        .env(
            "ASB_TAPE_MODE",
            match mode {
                crate::tape::Mode::Record => "record",
                crate::tape::Mode::Replay => "replay",
            },
        );
}

/// Resolve a record-mode upstream through provider-owned metadata.
pub(crate) fn tape_upstream(provider: &str, bridge_base_url: Option<&str>) -> Option<String> {
    if spec(provider).bridge_tool.is_some() {
        return bridge_base_url.map(strip_api_version);
    }
    std::env::var_os("ASB_PROVIDER_BASE_URL")
        .map(|value| value.to_string_lossy().trim().to_owned())
        .filter(|value| !value.is_empty())
        .or_else(|| {
            model_spec(provider).and_then(|spec| spec.url()).map(|url| {
                ["/api/tags", "/models", "/tags"]
                    .iter()
                    .find_map(|suffix| url.strip_suffix(suffix))
                    .unwrap_or(&url)
                    .to_owned()
            })
        })
        .map(|url| strip_api_version(&url))
}

fn strip_api_version(value: &str) -> String {
    value
        .trim_end_matches('/')
        .trim_end_matches("/v1")
        .to_owned()
}

#[cfg(test)]
#[path = "../../tests/unit/provider_boundaries.rs"]
mod boundary_tests;

#[cfg(test)]
mod tests {
    use super::{ProviderSpec, configure_tape, read_key_file, spec, tape_upstream};
    use crate::tape::Mode;
    use std::fs;

    #[test]
    fn responses_connections_route_tape_and_reject_incompatible_providers() {
        let live = super::endpoint_connection(
            "cli2key",
            super::offered_endpoints("cli2key", "fixture")[0].clone(),
            Some(("fixture-key", "http://127.0.0.1:10/v1")),
            None,
        )
        .unwrap();
        assert_eq!(live.base_url, "http://127.0.0.1:10/v1");
        assert_eq!(live.credential.as_deref(), Some("fixture-key"));
        for provider in ["cli2key", "openrouter", "openai", "ollama", "xai"] {
            let connection = super::endpoint_connection(
                provider,
                super::offered_endpoints(provider, "fixture")[0].clone(),
                Some(("live-secret", "http://127.0.0.1:10/v1")),
                Some((Mode::Replay, "http://127.0.0.1:20/v1")),
            )
            .unwrap();
            assert_eq!(connection.base_url, "http://127.0.0.1:20/v1");
            assert_eq!(connection.credential.as_deref(), Some("asb-replay"));
        }
        let offered = super::offered_endpoints("anthropic", "test-model");
        assert_eq!(
            crate::endpoint::select(&[crate::endpoint::RESPONSES], &offered),
            None
        );
        assert_eq!(
            super::offered_endpoints("opencode-zen", "gpt-5")[0].interface,
            crate::endpoint::Interface::OpenAiResponses
        );
        assert!(super::offered_endpoints("opencode-zen", "unknown").is_empty());
    }

    #[test]
    fn known_and_custom_providers_are_configurable() {
        assert_eq!(
            spec("cli2key"),
            ProviderSpec {
                name: "cli2key",
                credential: Some("CLI2KEY_API_KEY"),
                endpoint: Some("CLI2KEY_BASE_URL"),
                model_env: Some("CLI2KEY_MODEL"),
                tape_endpoint_envs: super::adapters::tape_endpoint_envs(),
                bridge_tool: Some("cli2key"),
            }
        );
        assert_eq!(spec("my-provider").name, "custom");
        assert_eq!(spec("LOCAL").name, "local");
        assert!(super::model_spec("local").is_none());
        assert_eq!(spec("opencode-zen").name, "opencode-zen");
        assert!(super::supports_free_models("opencode-zen"));
    }

    #[test]
    fn key_file_reader_trims_bounded_single_line_keys() {
        let path = std::env::temp_dir().join(format!("asb-provider-key-{}", std::process::id()));
        fs::write(&path, "  test-key\n").expect("key file writes");
        assert_eq!(read_key_file(path.clone()).as_deref(), Some("test-key"));
        fs::write(&path, "first\nsecond").expect("invalid key writes");
        assert!(read_key_file(path.clone()).is_none());
        let _ = fs::remove_file(path);
    }

    #[test]
    fn tape_routing_is_owned_by_the_provider_adapter() {
        assert_eq!(
            tape_upstream("cli2key", Some("http://127.0.0.1:3456/v1")).as_deref(),
            Some("http://127.0.0.1:3456")
        );
        assert_eq!(
            tape_upstream("openrouter", None).as_deref(),
            Some("https://openrouter.ai/api")
        );
        let mut command = std::process::Command::new("agent");
        configure_tape(
            &mut command,
            "openrouter",
            "http://127.0.0.1:3456/v1",
            Mode::Replay,
        );
        let values = command
            .get_envs()
            .filter_map(|(name, value)| {
                value.map(|value| {
                    (
                        name.to_string_lossy().into_owned(),
                        value.to_string_lossy().into_owned(),
                    )
                })
            })
            .collect::<std::collections::HashMap<_, _>>();
        assert_eq!(
            values.get("OPENROUTER_BASE_URL"),
            Some(&"http://127.0.0.1:3456/v1".to_owned())
        );
        assert_eq!(values.get("ASB_TAPE_MODE"), Some(&"replay".to_owned()));
    }
}
