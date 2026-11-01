//! Normalized token and step usage assembled from owning adapters.
//!
//! This deliberately is not a collector: agents own output parsing, providers
//! own their cache-token semantics, and replay owns the cassette evidence.
//! The module only normalizes and merges those independently observed facts.
use serde::{Deserialize, Serialize};
use std::{fs::File, io::Read, path::Path};

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct TokenUsage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cached_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
    pub source: Option<String>,
}

pub(crate) fn from_sources(
    provider: &str,
    agent: &str,
    stdout: &str,
    stderr: &str,
    cassette: Option<&Path>,
) -> TokenUsage {
    let mut usage = usage_from_environment(provider, agent);
    let inclusive = crate::providers::anthropic_input_includes_cache(provider);
    // Only the owning adapter can identify usage in its output. A missing
    // adapter or hook leaves usage unknown instead of parsing task text.
    let output_usage = adapter_output_usage(crate::agents::find(agent), stdout, stderr, inclusive);
    let mut cassette_usage = TokenUsage::default();
    if let Some(path) = cassette
        && let Ok(text) = read_bounded_text(path, 16 * 1024 * 1024)
    {
        cassette_usage = crate::proxy::usage::from_cassette(&text, inclusive);
    }
    if cassette_usage.input_tokens.is_some() && cassette_usage.output_tokens.is_some() {
        usage = usage.merge_with_source(cassette_usage, "cassette");
    } else {
        usage = usage.merge_with_source(output_usage, "agent-output");
    }
    usage.with_derived_total()
}

fn adapter_output_usage(
    adapter: Option<&dyn crate::agents::AgentAdapter>,
    stdout: &str,
    stderr: &str,
    inclusive: bool,
) -> TokenUsage {
    adapter
        .and_then(|adapter| adapter.output_usage(stdout, stderr, inclusive))
        .unwrap_or_default()
}

/// A complete run's authoritative counter belongs to the selected agent
/// adapter. It is never inferred from generic text matches.
pub(crate) fn steps_from_agent(agent: &str, stdout: &str, stderr: &str) -> Option<u64> {
    crate::agents::find(agent)?.step_count(stdout, stderr)
}

pub(crate) fn with_artifact_usage(
    mut usage: TokenUsage,
    artifact: Option<TokenUsage>,
) -> TokenUsage {
    if let Some(artifact) = artifact {
        let complete = artifact.input_tokens.is_some() && artifact.output_tokens.is_some();
        match usage.source.as_deref() {
            Some(source) if source.contains("cassette") => {}
            None | Some("agent-output") if complete => {
                usage = TokenUsage::default()
                    .merge_with_source(artifact, "agent-artifact")
                    .with_derived_total();
            }
            _ => {
                usage = usage
                    .merge_with_source(artifact, "agent-artifact")
                    .with_derived_total();
            }
        }
    }
    usage
}

/// Read a bounded machine-readable agent artifact (for example mini-SWE's
/// trajectory) and normalize any usage events it contains.
#[cfg(test)]
fn from_path(path: &Path) -> TokenUsage {
    read_bounded_text(path, 16 * 1024 * 1024)
        .map(|text| usage_from_text(&text).with_derived_total())
        .unwrap_or_default()
}

impl TokenUsage {
    pub(crate) fn merge(self, other: Self) -> Self {
        Self {
            input_tokens: self.input_tokens.or(other.input_tokens),
            output_tokens: self.output_tokens.or(other.output_tokens),
            cached_tokens: self.cached_tokens.or(other.cached_tokens),
            // Agent artifacts occasionally report a partial or stale total
            // (mini-SWE has emitted a small per-step total).  Keep the
            // largest observed total until the canonical input+output total
            // is derived below instead of letting a later artifact overwrite
            // the complete run.
            total_tokens: match (self.total_tokens, other.total_tokens) {
                (Some(left), Some(right)) => Some(left.max(right)),
                (Some(value), None) | (None, Some(value)) => Some(value),
                (None, None) => None,
            },
            source: self.source.or(other.source),
        }
    }

    pub(crate) fn add(self, other: Self) -> Self {
        Self {
            input_tokens: add_values(self.input_tokens, other.input_tokens),
            output_tokens: add_values(self.output_tokens, other.output_tokens),
            cached_tokens: add_values(self.cached_tokens, other.cached_tokens),
            total_tokens: add_values(self.total_tokens, other.total_tokens),
            source: self.source.or(other.source),
        }
    }

    pub(crate) fn merge_with_source(self, other: Self, source: &str) -> Self {
        let found = other.any();
        let mut merged = self.merge(other);
        if found {
            merged.source = Some(match merged.source.take() {
                Some(existing) if existing != source => format!("{existing}+{source}"),
                _ => source.into(),
            });
        }
        merged
    }

    pub(crate) fn with_derived_total(mut self) -> Self {
        // Cached input is a subset of input tokens, so total is input+output
        // rather than input+output+cached.  Prefer this canonical value over
        // provider-specific totals whenever both components are available.
        if let Some((input, output)) = self.input_tokens.zip(self.output_tokens) {
            self.total_tokens = Some(input.saturating_add(output));
        }
        self
    }

    pub(crate) fn any(&self) -> bool {
        self.input_tokens.is_some()
            || self.output_tokens.is_some()
            || self.cached_tokens.is_some()
            || self.total_tokens.is_some()
    }
}

fn usage_from_environment(provider: &str, agent: &str) -> TokenUsage {
    let mut prefixes = vec!["ASB".to_owned()];
    for value in [provider, agent] {
        let normalized = value
            .chars()
            .map(|character| {
                if character.is_ascii_alphanumeric() {
                    character.to_ascii_uppercase()
                } else {
                    '_'
                }
            })
            .collect::<String>();
        if !normalized.is_empty() && !prefixes.iter().any(|prefix| prefix == &normalized) {
            prefixes.push(normalized);
        }
    }
    let lookup = |suffixes: &[&str]| {
        prefixes.iter().find_map(|prefix| {
            suffixes
                .iter()
                .find_map(|suffix| env_u64(&format!("{prefix}_{suffix}")))
        })
    };
    let input = lookup(&["INPUT_TOKENS", "PROMPT_TOKENS", "INPUT_TOKEN_COUNT"]);
    let output = lookup(&["OUTPUT_TOKENS", "COMPLETION_TOKENS", "OUTPUT_TOKEN_COUNT"]);
    let cached = lookup(&[
        "CACHED_TOKENS",
        "CACHE_READ_INPUT_TOKENS",
        "PROMPT_CACHED_TOKENS",
    ]);
    let total = lookup(&["TOTAL_TOKENS", "TOKEN_COUNT"]);
    let usage = TokenUsage {
        input_tokens: input,
        output_tokens: output,
        cached_tokens: cached,
        total_tokens: total,
        source: None,
    };
    if usage.any() {
        TokenUsage {
            source: Some("environment".into()),
            ..usage
        }
    } else {
        usage
    }
}

pub(crate) fn usage_from_text(text: &str) -> TokenUsage {
    let mut usage = TokenUsage::default();
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(text.trim()) {
        return usage_from_json(&value);
    }
    for line in text.lines() {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(line.trim()) {
            usage = usage.add(usage_from_json(&value));
            continue;
        }
        for value in embedded_json_values(line) {
            usage = usage.add(usage_from_json(&value));
        }
        // JSON fields are already accounted for above.  Avoid treating their
        // `"name":number` members as a second key/value event.
        if !line.contains(['{', '}']) {
            usage = usage.add(usage_from_key_values(line));
        }
    }
    usage
}

/// Extract complete JSON objects from agent prefixes such as `event: {...}`.
/// Agents frequently mix human-readable progress text with JSON usage events;
/// requiring a whole line to be JSON made token accounting work only for the
/// agent that emits a pure JSON stream.
pub(super) fn embedded_json_values(text: &str) -> Vec<serde_json::Value> {
    let mut values = Vec::new();
    let mut start = None;
    let mut depth = 0_u32;
    let mut in_string = false;
    let mut escaped = false;
    for (index, character) in text.char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                in_string = false;
            }
            continue;
        }
        match character {
            '"' => in_string = true,
            '{' if depth == 0 => {
                start = Some(index);
                depth = 1;
            }
            '{' if depth > 0 => depth += 1,
            '}' if depth > 0 => {
                depth -= 1;
                if depth == 0
                    && let Some(start) = start.take()
                    && let Ok(value) = serde_json::from_str(&text[start..=index])
                {
                    values.push(value);
                }
            }
            _ => {}
        }
    }
    values
}

pub(crate) fn usage_from_json(value: &serde_json::Value) -> TokenUsage {
    let mut usage = TokenUsage::default();
    if let Some(object) = value.as_object() {
        let short_usage = object.contains_key("total")
            || (object.contains_key("input") && object.contains_key("output"))
            || object.contains_key("cached")
            || object.contains_key("cacheRead");
        usage.input_tokens = find_number(
            object,
            &[
                "input_tokens",
                "prompt_tokens",
                "inputTokens",
                "promptTokens",
                "input_token_count",
                "prompt_token_count",
                "promptTokenCount",
                "inputTokenCount",
            ],
        )
        .or_else(|| {
            short_usage
                .then(|| find_number(object, &["input", "prompt"]))
                .flatten()
        });
        usage.output_tokens = find_number(
            object,
            &[
                "output_tokens",
                "completion_tokens",
                "outputTokens",
                "completionTokens",
                "output_token_count",
                "completion_token_count",
                "candidatesTokenCount",
                "outputTokenCount",
            ],
        )
        .or_else(|| {
            short_usage
                .then(|| find_number(object, &["output", "completion"]))
                .flatten()
        });
        usage.cached_tokens = find_number(
            object,
            &[
                "cached_tokens",
                "cached_input_tokens",
                "cache_read_input_tokens",
                "cachedTokens",
                "cache_read_tokens",
                "cacheRead",
                "cache_read",
                "cacheReadInputTokens",
                "cacheCreationInputTokens",
                "cachedContentTokenCount",
            ],
        )
        .or_else(|| {
            short_usage
                .then(|| find_number(object, &["cached"]))
                .flatten()
        });
        usage.total_tokens = find_number(
            object,
            &[
                "total_tokens",
                "totalTokens",
                "token_count",
                "tokens_used",
                "totalTokenCount",
            ],
        )
        .or_else(|| {
            short_usage
                .then(|| find_number(object, &["total"]))
                .flatten()
        });
        for (name, child) in object {
            if ![
                "tokens",
                "input_tokens",
                "prompt_tokens",
                "inputTokens",
                "promptTokens",
                "input_token_count",
                "prompt_token_count",
                "promptTokenCount",
                "inputTokenCount",
                "output_tokens",
                "completion_tokens",
                "outputTokens",
                "completionTokens",
                "output_token_count",
                "completion_token_count",
                "candidatesTokenCount",
                "outputTokenCount",
                "cached_tokens",
                "cached_input_tokens",
                "cache_read_input_tokens",
                "cache_creation_input_tokens",
                "cachedTokens",
                "cache_read_tokens",
                "cacheRead",
                "cache_read",
                "cacheReadInputTokens",
                "cacheCreationInputTokens",
                "cachedContentTokenCount",
                "total_tokens",
                "totalTokens",
                "token_count",
                "tokens_used",
                "totalTokenCount",
                "input",
                "prompt",
                "output",
                "completion",
                "cached",
                "cache_creation_input_tokens",
                "cacheCreationInputTokens",
                "total",
            ]
            .contains(&name.as_str())
            {
                usage = usage.merge(usage_from_json(child));
            }
        }
    } else if let Some(array) = value.as_array() {
        for child in array {
            usage = usage.merge(usage_from_json(child));
        }
    } else if let Some(text) = value.as_str()
        && let Ok(nested) = serde_json::from_str::<serde_json::Value>(text)
    {
        usage = usage.merge(usage_from_json(&nested));
    }
    usage
}

fn add_values(left: Option<u64>, right: Option<u64>) -> Option<u64> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.saturating_add(right)),
        (Some(value), None) | (None, Some(value)) => Some(value),
        (None, None) => None,
    }
}

fn find_number(object: &serde_json::Map<String, serde_json::Value>, names: &[&str]) -> Option<u64> {
    names.iter().find_map(|name| {
        object.get(*name).and_then(|value| {
            value
                .as_u64()
                .or_else(|| value.as_str().and_then(|text| text.parse().ok()))
        })
    })
}

fn usage_from_key_values(line: &str) -> TokenUsage {
    let mut usage = TokenUsage::default();
    let normalized = line.replace(['=', ':', ',', ';', '{', '}', '[', ']'], " ");
    let tokens = normalized.split_whitespace().collect::<Vec<_>>();
    for (index, token) in tokens.iter().enumerate() {
        let key = token
            .trim_matches(['"', '\'', '.', ','])
            .to_ascii_lowercase()
            .replace(['-', '_'], "");
        if matches!(key.as_str(), "sent" | "received") {
            if let Some(number) = index
                .checked_sub(1)
                .and_then(|previous| tokens.get(previous))
                .and_then(|value| parse_token_count(value.trim_matches(['"', '\''])))
            {
                if key == "sent" {
                    usage.input_tokens = Some(number);
                } else {
                    usage.output_tokens = Some(number);
                }
            }
            continue;
        }
        let (key, number) = if let Some(number) = tokens
            .get(index + 1)
            .and_then(|value| parse_token_count(value.trim_matches(['"', '\''])))
        {
            (key, number)
        } else if let Some(number) = tokens
            .get(index + 2)
            .and_then(|value| parse_token_count(value.trim_matches(['"', '\''])))
            && matches!(
                key.as_str(),
                "input" | "output" | "cache" | "cached" | "total"
            )
        {
            let compound = tokens[index + 1]
                .to_ascii_lowercase()
                .replace(['-', '_'], "");
            let key = format!("{key}{compound}");
            (key, number)
        } else {
            continue;
        };
        match key.as_str() {
            "input" | "inputtokens" | "inputtoken" | "prompt" | "prompttokens"
            | "prompttokencount" | "inputtokencount" => usage.input_tokens = Some(number),
            "output"
            | "outputtokens"
            | "outputtoken"
            | "completion"
            | "completiontokens"
            | "candidatestokencount"
            | "outputtokencount" => usage.output_tokens = Some(number),
            "cached"
            | "cachedtokens"
            | "cacheread"
            | "cachereadinputtokens"
            | "cachecreationinputtokens"
            | "cachedcontenttokencount" => usage.cached_tokens = Some(number),
            "total" | "totaltokens" | "totaltokencount" | "tokencount" | "tokensused" => {
                usage.total_tokens = Some(number)
            }
            _ => {}
        }
    }
    usage
}

fn parse_token_count(value: &str) -> Option<u64> {
    let value = value.trim_matches(['"', '\'', '.', ',']);
    let (number, multiplier) = match value
        .chars()
        .last()
        .map(|suffix| suffix.to_ascii_lowercase())
    {
        Some('k') => (&value[..value.len() - 1], 1_000.0),
        Some('m') => (&value[..value.len() - 1], 1_000_000.0),
        _ => (value, 1.0),
    };
    number.parse::<f64>().ok().and_then(|value| {
        let value = value * multiplier;
        (value.is_finite() && value >= 0.0 && value <= u64::MAX as f64)
            .then_some(value.round() as u64)
    })
}

fn read_bounded_text(path: &Path, max_bytes: u64) -> Result<String, std::io::Error> {
    let file = File::open(path)?;
    let mut bytes = Vec::new();
    file.take(max_bytes + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max_bytes {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "usage artifact exceeds size limit",
        ));
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn env_u64(name: &str) -> Option<u64> {
    std::env::var(name).ok()?.parse().ok()
}

#[cfg(test)]
#[path = "../tests/unit/token_usage.rs"]
mod normalization_tests;

#[cfg(test)]
#[path = "usage_mock_tests.rs"]
mod usage_mock_tests;

#[cfg(test)]
mod tests {
    use crate::proxy::usage::from_cassette;

    #[test]
    fn cassette_aggregates_stream_usage_once_per_request() {
        let call = |body: &str| {
            serde_json::json!({"request":{"url":"/v1/messages"},"response":{"status":200,"body":body}}).to_string()
        };
        let probe = call(r#"{"usage":{"input_tokens":29,"output_tokens":22}}"#);
        let stream = call(concat!(
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":0,\"output_tokens\":0}}}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"usage\":{\"input_tokens\":9326,\"output_tokens\":58,\"cache_read_input_tokens\":20}}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"usage\":{\"input_tokens\":9326,\"output_tokens\":58,\"cache_read_input_tokens\":20}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
        ));
        let usage = from_cassette(&format!("{probe}\n{stream}\n"), true);
        assert_eq!(
            (
                usage.input_tokens,
                usage.output_tokens,
                usage.cached_tokens,
                usage.total_tokens
            ),
            (Some(9355), Some(80), None, Some(9435))
        );
        // Never present just the probe as the total for a partially recorded run.
        let missing = call("data: {\"type\":\"message_stop\"}\n\n");
        assert_eq!(
            from_cassette(&format!("{probe}\n{missing}\n"), true).total_tokens,
            None
        );
        let anthropic = from_cassette(&format!("{probe}\n{stream}\n"), false);
        assert_eq!(anthropic.input_tokens, Some(9375));
        assert_eq!(anthropic.total_tokens, Some(9455));
        let truncated = call("data: {\"usage\":{\"input_tokens\":10,\"output_tokens\":3}}\n\n");
        assert_eq!(from_cassette(&truncated, false).total_tokens, None);
    }
}
