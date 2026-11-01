//! Live model discovery through provider model-list endpoints.

use crate::Error;
use crate::providers;
use crate::suggestions;
use serde_json::Value;
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};

mod transport;

const MAX_CATALOG: usize = 32 * 1024 * 1024;
// Cassettes are bounded inputs, but multi-step tool-using agent runs can
// legitimately exceed 64 MiB. Keep this aligned with the bounded cassette
// evidence policy while admitting recorded SWE-bench-sized tracks.
const MAX_CASSETTE_BYTES: u64 = 256 * 1024 * 1024;

/// Read replay input exactly once through a bounded, regular-file-only path.
/// FIFOs and symlinks are rejected before any potentially blocking read.
pub(crate) fn read_replay_cassette(path: &Path) -> Result<String, Error> {
    let metadata = std::fs::symlink_metadata(path).map_err(|error| {
        Error::Input(format!(
            "cannot inspect replay cassette {}: {error}",
            path.display()
        ))
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(Error::Input(
            "replay cassette must be a regular file".into(),
        ));
    }
    if metadata.len() > MAX_CASSETTE_BYTES {
        return Err(Error::Input("replay cassette exceeds 256 MiB".into()));
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // The inspection above is intentionally repeated after opening, but
        // O_NOFOLLOW closes the remaining symlink-swap window on Unix.
        options.custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW);
    }
    let file = options.open(path).map_err(|error| {
        Error::Input(format!(
            "cannot read replay cassette {}: {error}",
            path.display()
        ))
    })?;
    let opened = file.metadata().map_err(|error| {
        Error::Input(format!(
            "cannot inspect replay cassette {}: {error}",
            path.display()
        ))
    })?;
    if !opened.is_file() {
        return Err(Error::Input(
            "replay cassette must be a regular file".into(),
        ));
    }
    if opened.len() > MAX_CASSETTE_BYTES {
        return Err(Error::Input("replay cassette exceeds 256 MiB".into()));
    }
    let mut bytes = Vec::new();
    file.take(MAX_CASSETTE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| {
            Error::Input(format!(
                "cannot read replay cassette {}: {error}",
                path.display()
            ))
        })?;
    if bytes.len() as u64 > MAX_CASSETTE_BYTES {
        return Err(Error::Input("replay cassette exceeds 256 MiB".into()));
    }
    String::from_utf8(bytes).map_err(|_| Error::Input("replay cassette is not UTF-8".into()))
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ModelInfo {
    id: String,
    // None means the live catalog does not report this capability.
    tool_calling: Option<bool>,
}

/// List models for one provider, or all providers when no name is supplied.
pub struct ModelListOptions {
    pub provider: Option<String>,
    pub json: bool,
    pub free_only: bool,
}

pub fn list(options: ModelListOptions, output: &mut dyn Write) -> Result<(), Error> {
    let ModelListOptions {
        provider,
        json,
        free_only,
    } = options;
    if let Some(provider) = provider {
        if free_only && !providers::supports_free_models(&provider) {
            return Err(Error::Input(format!(
                "`--free` is supported for {}; query one of those providers or omit `--free`",
                providers::free_names()
                    .iter()
                    .map(|name| format!("`{name}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
        let result = discover(&provider, free_only)?;
        render_one(output, &provider, result, json)
    } else {
        let mut all = Vec::new();
        let names = if free_only {
            providers::free_names()
        } else {
            providers::names()
        };
        for name in names {
            match discover(name, free_only) {
                Ok(models) => all.push(((*name).to_owned(), Ok(models))),
                Err(error) => all.push(((*name).to_owned(), Err(error))),
            }
        }
        render_all(output, all, json)
    }
}

fn discover(provider: &str, free_only: bool) -> Result<Vec<ModelInfo>, Error> {
    discover_for_model(provider, free_only, None)
}

fn discover_for_model(
    provider: &str,
    free_only: bool,
    selected: Option<&str>,
) -> Result<Vec<ModelInfo>, Error> {
    let spec = providers::model_spec(provider).ok_or_else(|| {
        Error::Input(suggestions::unknown(
            "provider",
            provider,
            providers::names(),
        ))
    })?;
    if let Some(bridge) = providers::discovery_session(provider)? {
        return discover_session_models(spec, provider, free_only, bridge);
    }
    let public_catalog =
        providers::public_catalog(provider, providers::credential(provider).is_some());
    if let Some(credential) = spec.credential
        && !public_catalog
        && providers::credential(provider).is_none()
    {
        return Err(Error::Input(format!(
            "missing {credential} or ~/.api_key_{}",
            spec.name
        )));
    }
    let Some(url) = spec.url_for_model(selected) else {
        if let Some(message) = providers::catalog_url_error(provider) {
            return Err(Error::Input(message.into()));
        }
        let endpoint = spec.endpoint.unwrap_or("provider endpoint variable");
        return Err(Error::Input(format!("missing {endpoint}")));
    };
    let request_spec = if public_catalog {
        providers::ModelSpec {
            credential: None,
            ..spec
        }
    } else {
        spec
    };
    let body = fetch_pages(request_spec, &url)?;
    let body = providers::enrich_catalog(provider, body)?;
    parse_models(&body, spec.kind, provider, free_only)
}

fn discover_session_models(
    spec: providers::ModelSpec,
    provider: &str,
    free_only: bool,
    bridge: Box<dyn providers::ProviderSession>,
) -> Result<Vec<ModelInfo>, Error> {
    let mut session = Some(bridge);
    let bridge = session.as_deref().unwrap();
    let result = (|| {
        let url = format!("{}/models", bridge.base_url().trim_end_matches('/'));
        let body = fetch_authenticated(spec, &url, bridge.api_key())?;
        parse_models(&body, spec.kind, provider, free_only)
    })();
    providers::finish_result(result, &mut session)
}

/// Discover tool-capable models for interactive setup without rendering them.
pub(crate) fn discover_for_setup(provider: &str) -> Result<Vec<String>, Error> {
    discover(provider, false).map(|models| {
        models
            .into_iter()
            .filter(|model| model.tool_calling != Some(false))
            .map(|model| model.id)
            .collect()
    })
}

/// Require the model to be in the live catalog and reject explicit lack of
/// tool support. Return false when the catalog leaves that capability unknown.
pub fn ensure_tool_capable(
    provider: &str,
    model: &str,
    bridge: Option<(&str, &str)>,
) -> Result<bool, Error> {
    let models = if let Some((url, key)) = bridge {
        let spec = providers::model_spec(provider)
            .ok_or_else(|| Error::Input(format!("unknown provider `{provider}`")))?;
        let body =
            fetch_authenticated(spec, &format!("{}/models", url.trim_end_matches('/')), key)?;
        parse_models(&body, spec.kind, provider, false)?
    } else {
        discover_for_model(provider, false, Some(model))?
    };
    match models.into_iter().find(|candidate| candidate.id == model) {
        Some(candidate) if candidate.tool_calling != Some(false) => {
            Ok(candidate.tool_calling.is_some())
        }
        Some(_) => Err(Error::Input(format!(
            "model `{model}` for provider `{provider}` does not support tool-enabled generation according to the provider catalog; choose another model"
        ))),
        None => Err(Error::Input(format!(
            "model `{model}` was not returned by provider `{provider}`; run `asb provider models {provider}` and choose a listed tool-capable model"
        ))),
    }
}

/// Validate a historical llmtape cassette in compatibility tests.
#[cfg(test)]
fn ensure_replay_model_recorded(cassette: &Path, model: &str) -> Result<(), Error> {
    crate::proxy::adapters::llmtape::replay::ensure_replay_model_recorded(cassette, model)
}

#[cfg(test)]
fn replay_interface(cassette: &Path) -> Result<Option<crate::endpoint::Interface>, Error> {
    crate::proxy::adapters::llmtape::replay::replay_interface(cassette)
}

pub(crate) fn fetch_pages(spec: providers::ModelSpec, url: &str) -> Result<Vec<u8>, Error> {
    collect_catalog_pages_for_schema(
        providers::catalog_schema(spec.name, spec.kind),
        url,
        |page| fetch_config(curl_config(spec, page, None)?, spec.name),
    )
    .map_err(|error| Error::Input(format!("{} model catalog: {error}", spec.name)))
}

// Some model catalogs are paginated. Keep requests,
// aggregate size, and cursor behavior bounded, and do not accept a URL from
// the response (which could redirect provider credentials to another host).
fn collect_catalog_pages_for_schema(
    schema: providers::CatalogSchema,
    base: &str,
    mut fetch_page: impl FnMut(&str) -> Result<Vec<u8>, Error>,
) -> Result<Vec<u8>, Error> {
    let collection = schema.collection;
    let mut merged = Vec::new();
    let mut cursor = None::<String>;
    let mut seen = std::collections::HashSet::new();
    let mut bytes = 0usize;
    for _ in 0..16 {
        let url = match &cursor {
            Some(token) => format!(
                "{base}{}{}={}",
                if base.contains('?') { '&' } else { '?' },
                match schema.pagination {
                    providers::CatalogPagination::Flagged { query, .. }
                    | providers::CatalogPagination::Token { query, .. } => query,
                    providers::CatalogPagination::None =>
                        unreachable!("unpaginated schema has no cursor"),
                },
                encode_query(token)
            ),
            None => base.to_owned(),
        };
        let body = fetch_page(&url)?;
        bytes += body.len();
        if bytes > MAX_CATALOG {
            return Err(Error::Input("provider model catalog exceeds 32 MiB".into()));
        }
        let value: Value = serde_json::from_slice(&body)
            .map_err(|_| Error::Input("provider returned invalid model JSON".into()))?;
        merged.extend(
            (if schema.allow_top_level_array && value.is_array() {
                Some(&value)
            } else {
                value.get(collection)
            })
            .and_then(Value::as_array)
            .ok_or_else(|| Error::Input("provider response has no model list".into()))?
            .iter()
            .cloned(),
        );
        let next = match schema.pagination {
            providers::CatalogPagination::Flagged { more, cursor, .. }
                if value.get(more).and_then(Value::as_bool) == Some(true) =>
            {
                Some(
                    value
                        .get(cursor)
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                        .ok_or_else(|| {
                            Error::Input("provider model catalog has no continuation cursor".into())
                        })?,
                )
            }
            providers::CatalogPagination::Token { field, .. } => value
                .get(field)
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty()),
            _ => None,
        };
        let Some(next) = next else {
            return serde_json::to_vec(&serde_json::json!({(collection): merged}))
                .map_err(|e| Error::Input(format!("cannot read provider model catalog: {e}")));
        };
        if next.len() > 1024 || !seen.insert(next.to_owned()) {
            return Err(Error::Input(
                "provider model catalog has an invalid or repeated continuation cursor".into(),
            ));
        }
        cursor = Some(next.to_owned());
    }
    Err(Error::Input(
        "provider model catalog exceeds 16 pages".into(),
    ))
}

#[cfg(test)]
fn collect_catalog_pages(
    kind: providers::ModelKind,
    base: &str,
    fetch_page: impl FnMut(&str) -> Result<Vec<u8>, Error>,
) -> Result<Vec<u8>, Error> {
    collect_catalog_pages_for_schema(providers::catalog_schema("", kind), base, fetch_page)
}

fn encode_query(value: &str) -> String {
    value
        .bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
                (byte as char).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect()
}

fn fetch_authenticated(spec: providers::ModelSpec, url: &str, key: &str) -> Result<Vec<u8>, Error> {
    if url.contains(['\n', '\r']) || key.contains(['\n', '\r']) {
        return Err(Error::Input(
            "provider endpoint or key contains a newline".into(),
        ));
    }
    let config = curl_config(spec, url, Some(key))?;
    fetch_config(config, spec.name)
}

pub(crate) fn fetch_config(config: String, provider: &str) -> Result<Vec<u8>, Error> {
    let mut command = Command::new("curl");
    command.current_dir(crate::project::network_invocation_dir());
    crate::project::apply_network_proxy(&mut command, std::path::Path::new("."))?;
    let io_timeout = transport::IO_TIMEOUT.as_secs().to_string();
    let max_response = transport::MAX_RESPONSE.to_string();
    command.args([
        "--disable",
        "--config",
        "-",
        "--fail",
        "--silent",
        "--show-error",
        "--connect-timeout",
        "10",
        "--max-time",
        &io_timeout,
        "--max-filesize",
        &max_response,
        "--proto",
        "=http,https",
        "--write-out",
        "%{stderr}\nASB_HTTP_STATUS=%{http_code}\nASB_PROXY_STATUS=%{http_connect}\nASB_CONTENT_TYPE=%{content_type}\n",
    ]);
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let captured = transport::capture(&mut command, config.as_bytes(), provider)?;
    let status = captured.status;
    let body = captured.stdout;
    let metadata = CurlMetadata::parse(&captured.stderr);
    if status.code() == Some(63) {
        return Err(Error::Input(format!(
            "model catalog for {provider} exceeds the 16 MiB response limit; retry later or contact the provider"
        )));
    }
    if let Some(code) = metadata.proxy_status.filter(|code| *code >= 400) {
        return Err(Error::Input(format!(
            "model discovery for {provider} failed: HTTP proxy returned {code} while establishing the connection. Check the project's proxy configuration and proxy authentication."
        )));
    }
    if let Some(code) = metadata.http_status.filter(|code| *code >= 300) {
        let help = match code {
            401 | 403 => "Check the provider API key and account access.",
            407 => "Check proxy authentication and the project's proxy configuration.",
            429 => "The service rate-limited the request; retry later.",
            500..=599 => {
                "The provider or proxy returned a server error; retry later and check the proxy."
            }
            300..=399 => {
                "Redirects are not followed for model discovery; check the configured endpoint."
            }
            _ => "Check the provider endpoint and proxy configuration.",
        };
        return Err(Error::Input(format!(
            "model discovery for {provider} returned HTTP {code}. {help}"
        )));
    }
    if !status.success() {
        let detail = match status.code() {
            Some(5) => "could not resolve the configured proxy; check the proxy address and DNS",
            Some(6) => "could not resolve the provider host; check DNS and the configured proxy",
            Some(7) => {
                "could not connect to the provider or proxy; check the endpoint and proxy settings"
            }
            Some(28) => "the request timed out; check the provider connection and proxy",
            Some(35 | 51 | 60 | 77 | 90) => {
                "TLS verification or negotiation failed; check certificates and any HTTPS proxy"
            }
            Some(56) => {
                "the provider or proxy connection was interrupted; check the proxy and retry"
            }
            _ => "the provider connection failed; check the endpoint, network, and proxy settings",
        };
        return Err(Error::Input(format!(
            "model discovery for {provider} failed (curl exit {}): {detail}.",
            status
                .code()
                .map_or_else(|| "signal".into(), |code| code.to_string())
        )));
    }
    if let Err(error) = serde_json::from_slice::<Value>(&body) {
        let kind = if error.is_eof() {
            "incomplete or truncated JSON"
        } else {
            "invalid JSON"
        };
        let content = metadata
            .content_type
            .as_deref()
            .map(|value| format!(", content type {value}"))
            .unwrap_or_default();
        return Err(Error::Input(format!(
            "model discovery for {provider} returned {kind} (HTTP {}{content}, {} bytes). A proxy or provider may have returned an error page or incomplete response; check proxy settings and retry.",
            metadata
                .http_status
                .map_or_else(|| "unknown".into(), |code| code.to_string()),
            body.len()
        )));
    }
    Ok(body)
}

#[derive(Default)]
struct CurlMetadata {
    http_status: Option<u16>,
    proxy_status: Option<u16>,
    content_type: Option<String>,
}

impl CurlMetadata {
    fn parse(stderr: &[u8]) -> Self {
        let text = String::from_utf8_lossy(stderr);
        let mut metadata = Self::default();
        for line in text.lines() {
            if let Some(value) = line.strip_prefix("ASB_HTTP_STATUS=") {
                metadata.http_status = value
                    .parse::<u16>()
                    .ok()
                    .filter(|code| (100..=599).contains(code));
            } else if let Some(value) = line.strip_prefix("ASB_PROXY_STATUS=") {
                metadata.proxy_status = value
                    .parse::<u16>()
                    .ok()
                    .filter(|code| (100..=599).contains(code));
            } else if let Some(value) = line.strip_prefix("ASB_CONTENT_TYPE=") {
                let mime = value.split(';').next().unwrap_or_default().trim();
                if !mime.is_empty() {
                    metadata.content_type = Some(
                        match mime.to_ascii_lowercase().as_str() {
                            "application/json" => "application/json",
                            "text/html" => "text/html",
                            "text/plain" => "text/plain",
                            "application/octet-stream" => "application/octet-stream",
                            _ => "other",
                        }
                        .to_owned(),
                    );
                }
            }
        }
        metadata
    }
}

pub(crate) fn curl_config(
    spec: providers::ModelSpec,
    url: &str,
    key_override: Option<&str>,
) -> Result<String, Error> {
    if url.contains('\n') || url.contains('\r') {
        return Err(Error::Input(
            "provider endpoint contains invalid newline characters".into(),
        ));
    }
    let mut config = format!("url = \"{}\"\n", escape_config(url));
    if let Some(credential) = spec.credential {
        let key = match key_override {
            Some(value) => value.to_owned(),
            None => providers::credential(spec.name).ok_or_else(|| {
                Error::Input(format!("missing {credential} or ~/.api_key_{}", spec.name))
            })?,
        };
        if key.contains('\n') || key.contains('\r') {
            return Err(Error::Input(format!(
                "{credential} contains invalid newline characters"
            )));
        }
        for header in providers::catalog_headers(spec.name, &key) {
            config.push_str(&format!("header = \"{}\"\n", escape_config(&header)));
        }
    }
    Ok(config)
}

pub(crate) fn escape_config(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn parse_models(
    body: &[u8],
    kind: providers::ModelKind,
    provider: &str,
    free_only: bool,
) -> Result<Vec<ModelInfo>, Error> {
    let value: Value = serde_json::from_slice(body)
        .map_err(|_| Error::Input("provider returned invalid model JSON".into()))?;
    let key = providers::catalog_schema(provider, kind).collection;
    let entries = value
        .get(key)
        .and_then(Value::as_array)
        .ok_or_else(|| Error::Input("provider response has no model list".into()))?;
    let mut models = entries
        .iter()
        .filter_map(|entry| {
            let id = entry
                .get("id")
                .or_else(|| entry.get("name"))
                .and_then(Value::as_str)?
                .strip_prefix("models/")
                .unwrap_or_else(|| {
                    entry
                        .get("id")
                        .or_else(|| entry.get("name"))
                        .and_then(Value::as_str)
                        .unwrap_or("")
                })
                .to_owned();
            let tool_calling = supports_tools(entry, kind, provider);
            (!free_only || is_free_model_entry(entry, &id, provider, kind))
                .then_some(ModelInfo { tool_calling, id })
        })
        .collect::<Vec<_>>();
    models.sort_unstable_by(|left, right| left.id.cmp(&right.id));
    models.dedup_by(|left, right| left.id == right.id);
    for model in &models {
        crate::selection_names::validate(&model.id, "model").map_err(Error::Input)?;
    }
    Ok(models)
}

fn is_free_model_entry(
    entry: &Value,
    model_id: &str,
    provider: &str,
    _kind: providers::ModelKind,
) -> bool {
    providers::free_model(provider, entry, model_id)
}

pub(crate) fn is_zero_price(value: Option<&Value>) -> bool {
    value.is_some_and(|value| {
        value
            .as_f64()
            .or_else(|| {
                value
                    .as_str()
                    .and_then(|text| text.trim().parse::<f64>().ok())
            })
            .is_some_and(|number| number == 0.0)
    })
}

fn supports_tools(entry: &Value, _kind: providers::ModelKind, provider: &str) -> Option<bool> {
    if let Some(result) = providers::catalog_tool_support(provider, entry) {
        return Some(result);
    }
    for key in [
        "tool_calling",
        "tool_calls",
        "supports_tool_calls",
        "supportsTools",
    ] {
        if let Some(value) = entry.get(key).and_then(Value::as_bool) {
            return Some(value);
        }
    }
    if let Some(value) = entry
        .get("capabilities")
        .and_then(|capabilities| {
            capabilities
                .get("tools")
                .or_else(|| capabilities.get("tool_calling"))
                .or_else(|| capabilities.get("function_calling"))
        })
        .and_then(|value| {
            value
                .as_bool()
                .or_else(|| value.get("supported").and_then(Value::as_bool))
        })
    {
        return Some(value);
    }
    if let Some(parameters) = entry.get("supported_parameters").and_then(Value::as_array) {
        return Some(
            parameters
                .iter()
                .filter_map(Value::as_str)
                .any(|parameter| {
                    matches!(
                        parameter,
                        "tools" | "tool_choice" | "functions" | "function_calling"
                    )
                }),
        );
    }
    // These APIs document tool use as a standard capability of their exposed
    // agent models, while their model list does not include a capability bit.
    providers::default_tool_support(provider)
}

fn render_one(
    output: &mut dyn Write,
    provider: &str,
    result: Vec<ModelInfo>,
    json: bool,
) -> Result<(), Error> {
    let names = model_ids(&result);
    if json {
        let value = serde_json::json!({"provider": provider, "models": names});
        serde_json::to_writer_pretty(&mut *output, &value)
            .map_err(|error| Error::Io(error.to_string()))?;
        writeln!(output).map_err(super::io_error)
    } else {
        writeln!(output, "{provider} models ({}):", names.len()).map_err(super::io_error)?;
        for model in names {
            writeln!(output, "{model}").map_err(super::io_error)?;
        }
        Ok(())
    }
}

fn render_all(
    output: &mut dyn Write,
    results: Vec<(String, Result<Vec<ModelInfo>, Error>)>,
    json: bool,
) -> Result<(), Error> {
    if json {
        let value = results
            .into_iter()
            .map(|(name, result)| {
                (
                    name,
                    match result {
                        Ok(models) => {
                            serde_json::json!({"status":"ok", "models":model_ids(&models)})
                        }
                        Err(error) => {
                            serde_json::json!({"status":"unavailable", "reason":error.to_string()})
                        }
                    },
                )
            })
            .collect::<serde_json::Map<_, _>>();
        serde_json::to_writer_pretty(&mut *output, &value)
            .map_err(|error| Error::Io(error.to_string()))?;
        return writeln!(output).map_err(super::io_error);
    }
    for (name, result) in results {
        match result {
            Ok(models) => {
                let models = model_ids(&models);
                writeln!(output, "{name} ({}):", models.len()).map_err(super::io_error)?;
                for model in models {
                    writeln!(output, "  {model}").map_err(super::io_error)?;
                }
            }
            Err(error) => {
                writeln!(output, "{name}: unavailable ({error})").map_err(super::io_error)?;
            }
        }
    }
    Ok(())
}

fn model_ids(models: &[ModelInfo]) -> Vec<String> {
    models
        .iter()
        .filter(|model| model.tool_calling != Some(false))
        .map(|model| model.id.clone())
        .collect()
}

#[cfg(test)]
#[path = "../tests/unit/model_provider_finish.rs"]
mod provider_finish_tests;

#[cfg(test)]
#[path = "../tests/unit/models.rs"]
mod coverage_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adapter_catalog_schema_drives_pagination_and_collection() {
        let schema = providers::CatalogSchema {
            collection: "records",
            allow_top_level_array: false,
            pagination: providers::CatalogPagination::Token {
                field: "continuation",
                query: "cursor",
            },
        };
        let mut urls = Vec::new();
        let body = collect_catalog_pages_for_schema(schema, "http://127.0.0.1/catalog", |url| {
            urls.push(url.to_owned());
            Ok(if urls.len() == 1 {
                br#"{"records":[{"id":"one"}],"continuation":"a/b?"}"#.to_vec()
            } else {
                br#"{"records":[{"id":"two"}]}"#.to_vec()
            })
        })
        .unwrap();
        assert!(urls[1].ends_with("?cursor=a%2Fb%3F"));
        let value: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["records"].as_array().unwrap().len(), 2);
        // A registered provider owns its schema even when a caller supplies
        // a different legacy fallback kind; unknown fixtures use the fallback.
        assert_eq!(
            providers::catalog_schema("gemini", providers::ModelKind::DataUnpaginated).collection,
            "models"
        );
        assert_eq!(
            providers::catalog_schema("fixture", providers::ModelKind::DataUnpaginated).collection,
            "data"
        );
    }

    #[test]
    fn catalog_explicit_capabilities_take_precedence_over_documented_defaults() {
        assert_eq!(
            supports_tools(
                &serde_json::json!({"capabilities":{"tools":{"supported":false}}}),
                providers::ModelKind::DataCursor,
                "anthropic"
            ),
            Some(false)
        );
        assert_eq!(
            supports_tools(
                &serde_json::json!({"supportedGenerationMethods":["embedContent"]}),
                providers::ModelKind::ModelsPageTokenApiKey,
                "gemini"
            ),
            Some(false)
        );
        assert_eq!(
            supports_tools(
                &serde_json::json!({"supportedGenerationMethods":["generateContent"]}),
                providers::ModelKind::ModelsPageTokenApiKey,
                "gemini"
            ),
            None
        );
        assert_eq!(
            supports_tools(
                &serde_json::json!({"capabilities":{"function_calling":false}}),
                providers::ModelKind::DataUnpaginated,
                "mistral"
            ),
            Some(false)
        );
        assert_eq!(
            supports_tools(
                &serde_json::json!({"supportsTools":true}),
                providers::ModelKind::ModelsPageTokenBearer,
                "fireworks"
            ),
            Some(true)
        );
    }

    #[test]
    fn catalog_pagination_uses_encoded_cursors_and_collects_every_page() {
        for (kind, first, second, query) in [
            (
                providers::ModelKind::DataCursor,
                r#"{"data":[{"id":"one"}],"has_more":true,"last_id":"a/b?"}"#,
                r#"{"data":[{"id":"two"}],"has_more":false}"#,
                "after_id=a%2Fb%3F",
            ),
            (
                providers::ModelKind::ModelsPageTokenApiKey,
                r#"{"models":[{"name":"models/one"}],"nextPageToken":"a/b?"}"#,
                r#"{"models":[{"name":"models/two"}]}"#,
                "pageToken=a%2Fb%3F",
            ),
            (
                providers::ModelKind::ModelsPageTokenBearer,
                r#"{"models":[{"name":"accounts/fireworks/models/one"}],"nextPageToken":"a/b?"}"#,
                r#"{"models":[{"name":"accounts/fireworks/models/two"}]}"#,
                "pageToken=a%2Fb%3F",
            ),
        ] {
            let mut calls = Vec::new();
            let body = collect_catalog_pages(kind, "http://127.0.0.1/v1/models", |url| {
                calls.push(url.to_owned());
                Ok(if calls.len() == 1 { first } else { second }
                    .as_bytes()
                    .to_vec())
            })
            .unwrap();
            assert_eq!(calls.len(), 2);
            assert!(calls[1].ends_with(query));
            let parsed = parse_models(&body, kind, "fixture", false).unwrap();
            if kind == providers::ModelKind::ModelsPageTokenBearer {
                assert_eq!(
                    model_ids(&parsed),
                    vec![
                        "accounts/fireworks/models/one",
                        "accounts/fireworks/models/two"
                    ]
                );
            } else {
                assert_eq!(model_ids(&parsed), vec!["one", "two"]);
            }
        }
    }

    #[test]
    fn together_catalog_accepts_documented_top_level_array() {
        let body = collect_catalog_pages(
            providers::ModelKind::ArrayUnpaginated,
            "http://localhost/models",
            |_| Ok(br#"[{"id":"fixture/chat","type":"chat"}]"#.to_vec()),
        )
        .unwrap();
        let parsed = parse_models(
            &body,
            providers::ModelKind::ArrayUnpaginated,
            "together",
            false,
        )
        .unwrap();
        assert_eq!(model_ids(&parsed), vec!["fixture/chat"]);
        assert_eq!(parsed[0].tool_calling, None);
    }

    #[test]
    fn catalog_pagination_rejects_repeated_and_missing_cursors() {
        let repeated = collect_catalog_pages(
            providers::ModelKind::ModelsPageTokenApiKey,
            "http://localhost/models",
            |_| Ok(br#"{"models":[],"nextPageToken":"same"}"#.to_vec()),
        );
        assert!(repeated.unwrap_err().to_string().contains("repeated"));
        let missing = collect_catalog_pages(
            providers::ModelKind::DataCursor,
            "http://localhost/models",
            |_| Ok(br#"{"data":[],"has_more":true}"#.to_vec()),
        );
        assert!(
            missing
                .unwrap_err()
                .to_string()
                .contains("continuation cursor")
        );
    }

    fn replay_fixture(contents: &str, check: impl FnOnce(&Path)) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "asb-replay-validation-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::write(&path, contents).unwrap();
        check(&path);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn replay_detects_recorded_interface_without_live_metadata() {
        for (path, expected) in [
            ("/v1/responses", crate::endpoint::Interface::OpenAiResponses),
            (
                "/api/v1/chat/completions?x=1",
                crate::endpoint::Interface::OpenAiChat,
            ),
            (
                "/v1/messages?beta=true",
                crate::endpoint::Interface::AnthropicMessages,
            ),
        ] {
            let cassette = serde_json::json!({"request":{"url":path}}).to_string();
            replay_fixture(&cassette, |file| {
                assert_eq!(replay_interface(file).unwrap(), Some(expected))
            });
        }
    }

    #[test]
    fn malformed_and_wrong_shape_model_catalogs_are_rejected() {
        for (body, message) in [
            (&b"not json"[..], "invalid model JSON"),
            (&b"{}"[..], "no model list"),
            (&b"{\"data\":{}}"[..], "no model list"),
        ] {
            let error = parse_models(body, providers::ModelKind::DataUnpaginated, "openai", false)
                .unwrap_err();
            assert!(error.to_string().contains(message));
        }
    }

    #[test]
    fn model_tool_capabilities_accept_supported_metadata_shapes() {
        let models = parse_models(
            br#"{"data":[
          {"id":"a","tool_calling":true},
          {"id":"b","tool_calls":true},
          {"id":"c","supports_tool_calls":true},
          {"id":"d","capabilities":{"tool_calling":true}},
          {"id":"e","supported_parameters":["function_calling"]},
          {"id":"f","supported_parameters":["tool_choice"]},
          {"id":"ignored","tool_calling":false},
          {"supported_parameters":["tools"]}
        ]}"#,
            providers::ModelKind::DataUnpaginated,
            "openrouter",
            false,
        )
        .unwrap();
        assert_eq!(model_ids(&models), vec!["a", "b", "c", "d", "e", "f"]);
    }

    #[test]
    fn free_pricing_requires_both_input_and_output_to_be_zero() {
        let models = parse_models(
            br#"{"data":[
          {"id":"both-zero","tool_calling":true,"pricing":{"prompt":0,"completion":"0.0"}},
          {"id":"missing","tool_calling":true,"pricing":{"prompt":0}},
          {"id":"paid-output","tool_calling":true,"pricing":{"prompt":0,"completion":1}},
          {"id":"nonnumeric","tool_calling":true,"pricing":{"prompt":"free","completion":0}}
        ]}"#,
            providers::ModelKind::DataUnpaginated,
            "openrouter",
            true,
        )
        .unwrap();
        assert_eq!(model_ids(&models), vec!["both-zero"]);
    }

    #[test]
    fn curl_configuration_rejects_newlines_and_escapes_credentials() {
        let spec = providers::model_spec("openai").unwrap();
        let output = curl_config(
            spec,
            "http://127.0.0.1:1234/v1/models",
            Some("fake\"key\\value"),
        )
        .unwrap();
        assert!(output.contains(r#"Authorization: Bearer fake\"key\\value"#));
        assert!(curl_config(spec, "http://127.0.0.1/\nheader=bad", Some("fixture")).is_err());
        assert!(curl_config(spec, "http://127.0.0.1/", Some("fixture\r\nheader=bad")).is_err());
    }

    #[test]
    fn replay_skips_bodyless_requests_before_tool_call() {
        replay_fixture(
            concat!(
                "{\"request\":{\"method\":\"GET\",\"body\":null}}\n",
                "{\"request\":{\"body\":{\"model\":\"test-model\",\"tools\":[{}]}}}\n"
            ),
            |path| ensure_replay_model_recorded(path, "test-model").unwrap(),
        );
    }

    #[test]
    fn replay_rejects_invalid_jsonl_and_invalid_encoded_request_body() {
        for (fixture, message) in [
            ("not json\n", "invalid JSONL"),
            (
                "{\"request\":{\"body\":\"not-json\"}}\n",
                "request body that is not valid JSON",
            ),
        ] {
            replay_fixture(fixture, |path| {
                assert!(
                    ensure_replay_model_recorded(path, "test-model")
                        .unwrap_err()
                        .to_string()
                        .contains(message)
                );
            });
        }
    }

    #[test]
    fn replay_accepts_matching_models_without_a_tool_request() {
        replay_fixture(
            "{\"request\":{\"body\":{\"model\":\"test-model\",\"tools\":[]}}}\n",
            |path| {
                ensure_replay_model_recorded(path, "test-model").unwrap();
            },
        );
    }

    #[test]
    fn replay_accepts_tool_choice_and_legacy_function_requests() {
        for body in [
            r#"{"model":"test-model","tool_choice":"auto"}"#,
            r#"{"model":"test-model","functions":[{}]}"#,
        ] {
            replay_fixture(&format!("{{\"request\":{{\"body\":{body}}}}}\n"), |path| {
                ensure_replay_model_recorded(path, "test-model").unwrap();
            });
        }
    }

    #[test]
    fn replay_rejects_cassettes_over_the_size_limit() {
        replay_fixture("", |path| {
            std::fs::OpenOptions::new()
                .write(true)
                .open(path)
                .unwrap()
                .set_len(MAX_CASSETTE_BYTES + 1)
                .unwrap();
            assert!(
                ensure_replay_model_recorded(path, "test-model")
                    .unwrap_err()
                    .to_string()
                    .contains("256 MiB")
            );
        });
    }

    #[cfg(unix)]
    #[test]
    fn replay_reader_rejects_symlinks_and_fifos_before_reading() {
        use std::os::unix::fs::symlink;

        let root = std::env::temp_dir().join(format!(
            "asb-replay-input-type-{}",
            crate::tools::runtime::unique_id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let target = root.join("recording.jsonl");
        let link = root.join("recording-link.jsonl");
        std::fs::write(&target, "{\"request\":{}}\n").unwrap();
        symlink(&target, &link).unwrap();
        assert!(
            read_replay_cassette(&link)
                .unwrap_err()
                .to_string()
                .contains("regular file")
        );

        let fifo = root.join("recording.fifo");
        let status = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap();
        assert!(status.success());
        assert!(
            read_replay_cassette(&fifo)
                .unwrap_err()
                .to_string()
                .contains("regular file")
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn parses_openai_and_gemini_model_shapes() {
        assert_eq!(
            parse_models(
                br#"{"data":[{"id":"z","supported_parameters":["tools"]},{"id":"a","supported_parameters":["tools"]},{"id":"a","supported_parameters":["tools"]}]}"#,
                providers::ModelKind::DataUnpaginated,
                "openai",
                false,
            )
            .unwrap()
            .iter()
            .map(|model| model.id.clone())
            .collect::<Vec<_>>(),
            vec!["a", "z"]
        );
        assert_eq!(
            parse_models(
                br#"{"models":[{"name":"models/gemini-2.0","capabilities":{"tools":true}}]}"#,
                providers::ModelKind::ModelsPageTokenApiKey,
                "gemini",
                false,
            )
            .unwrap()
            .iter()
            .map(|model| model.id.clone())
            .collect::<Vec<_>>(),
            vec!["gemini-2.0"]
        );
    }

    #[test]
    fn retains_models_with_unknown_tool_calling_metadata() {
        let models = parse_models(
            br#"{"data":[{"id":"plain"},{"id":"tools","supported_parameters":["tools"]},{"id":"flag","capabilities":{"tools":true}}]}"#,
            providers::ModelKind::DataUnpaginated,
            "openrouter",
            false,
        )
        .expect("model response parses");
        assert_eq!(model_ids(&models), vec!["flag", "plain", "tools"]);
        assert_eq!(
            models
                .iter()
                .find(|m| m.id == "plain")
                .unwrap()
                .tool_calling,
            None
        );
    }

    #[test]
    fn free_filter_accepts_openrouter_free_models_only() {
        let models = parse_models(
            br#"{"data":[
                {"id":"paid","supported_parameters":["tools"],"pricing":{"prompt":"0.000001","completion":"0.000002"}},
                {"id":"suffix:free","supported_parameters":["tools"],"pricing":{"prompt":"0","completion":"0"}},
                {"id":"zero-pricing","supported_parameters":["tools"],"pricing":{"prompt":"0","completion":"0"}},
                {"id":"no-tools:free","pricing":{"prompt":"0","completion":"0"}}
            ]}"#,
            providers::ModelKind::DataUnpaginated,
            "openrouter",
            true,
        )
        .expect("model response parses");
        assert_eq!(
            model_ids(&models),
            vec!["no-tools:free", "suffix:free", "zero-pricing"]
        );
    }

    #[test]
    fn free_filter_treats_ollama_models_as_local() {
        let models = parse_models(
            br#"{"models":[{"name":"local-a","capabilities":{"tools":true}},{"name":"local-b","capabilities":{"tools":true}}]}"#,
            providers::ModelKind::ModelsUnpaginated,
            "ollama",
            true,
        )
        .expect("model response parses");
        assert_eq!(model_ids(&models), vec!["local-a", "local-b"]);
    }

    #[test]
    fn ollama_capability_arrays_only_accept_explicit_tools() {
        let models = parse_models(
            br#"{"models":[
              {"name":"qwen3:0.6b","capabilities":["completion","tools","thinking"]},
              {"name":"embedding","capabilities":["embedding"]},
              {"name":"plain","capabilities":["completion"]},
              {"name":"unknown"},
              {"name":"wrong","capabilities":["tool"]}
            ]}"#,
            providers::ModelKind::ModelsUnpaginated,
            "ollama",
            true,
        )
        .unwrap();
        assert_eq!(model_ids(&models), vec!["qwen3:0.6b", "unknown"]);
    }

    #[test]
    fn opencode_zen_catalog_is_public_and_free_models_are_suffix_marked() {
        let models = parse_models(
            br#"{"data":[{"id":"paid-model"},{"id":"free-model-free"}]}"#,
            providers::ModelKind::DataUnpaginated,
            "opencode-zen",
            true,
        )
        .expect("model response parses");
        assert_eq!(model_ids(&models), vec!["free-model-free"]);
        assert!(
            providers::model_spec("opencode-zen")
                .unwrap()
                .credential
                .is_some()
        );
    }

    #[test]
    fn free_flag_parses_with_json_and_provider_in_any_order() {
        let options = ModelListOptions {
            provider: Some("openrouter".into()),
            json: true,
            free_only: true,
        };
        assert_eq!(options.provider.as_deref(), Some("openrouter"));
        assert!(options.json && options.free_only);
    }

    #[test]
    fn free_flag_rejects_providers_without_free_catalog_metadata() {
        let error = list(
            ModelListOptions {
                provider: Some("openai".into()),
                json: false,
                free_only: true,
            },
            &mut Vec::new(),
        )
        .expect_err("paid provider cannot be classified as free");
        assert!(error.to_string().contains("openrouter`"));
    }

    #[test]
    fn replay_requires_a_matching_tool_request() {
        let path = std::env::temp_dir().join(format!("asb-tool-cassette-{}", std::process::id()));
        std::fs::write(
            &path,
            r#"{"request":{"body":{"model":"tool-model","tools":[{"type":"function"}]}}}
"#,
        )
        .expect("cassette writes");
        ensure_replay_model_recorded(&path, "tool-model").expect("model cassette accepted");
        let error =
            ensure_replay_model_recorded(&path, "other-model").expect_err("wrong model rejected");
        assert!(error.to_string().contains("no request for model"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn replay_accepts_json_encoded_request_bodies_and_provider_namespaces() {
        let path =
            std::env::temp_dir().join(format!("asb-tool-cassette-json-{}", std::process::id()));
        std::fs::write(
            &path,
            r#"{"request":{"body":"{\"model\":\"openrouter/tool-model\",\"tools\":[{\"type\":\"function\"}]}"},"response":{"status":200}}
"#,
        )
        .expect("cassette writes");
        ensure_replay_model_recorded(&path, "tool-model").expect("namespaced model accepted");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn unsupported_provider_suggests_a_close_match() {
        let error = discover("openruter", false).expect_err("typo rejected");
        assert!(error.to_string().contains("did you mean `openrouter`?"));
    }
}
