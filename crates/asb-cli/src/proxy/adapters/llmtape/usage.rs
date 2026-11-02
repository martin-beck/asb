//! Replay-cassette usage evidence.
//!
//! The proxy owns the request/response wire records. This module converts only
//! complete successful cassette responses into normalized usage observations.

use crate::usage::TokenUsage;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;

// Bound individual wire records while allowing long recordings with many calls.
const MAX_RECORD_BYTES: u64 = 16 * 1024 * 1024;

pub(crate) fn from_file(path: &Path, input_includes_cache: bool) -> TokenUsage {
    let Ok(file) = std::fs::File::open(path) else {
        return TokenUsage::default();
    };
    let mut reader = BufReader::new(file);
    let lines = std::iter::from_fn(move || {
        let mut bytes = Vec::new();
        match reader
            .by_ref()
            .take(MAX_RECORD_BYTES + 1)
            .read_until(b'\n', &mut bytes)
        {
            Ok(0) => None,
            Ok(_) if bytes.len() as u64 <= MAX_RECORD_BYTES => {
                Some(String::from_utf8(bytes).map_err(|_| ()))
            }
            _ => Some(Err(())),
        }
    });
    from_lines(lines, input_includes_cache)
}

/// A cassette has one response per request. SSE usage snapshots within one
/// response are cumulative, not separate calls; only sum across responses.
pub(crate) fn from_cassette(text: &str, input_includes_cache: bool) -> TokenUsage {
    from_lines(
        text.lines().map(|line| Ok(line.to_owned())),
        input_includes_cache,
    )
}

fn from_lines(
    lines: impl Iterator<Item = Result<String, ()>>,
    input_includes_cache: bool,
) -> TokenUsage {
    let mut sum = TokenUsage::default();
    let mut cache_complete = true;
    for line in lines {
        let Ok(line) = line else {
            return TokenUsage::default();
        };
        let Ok(record) = serde_json::from_str::<serde_json::Value>(&line) else {
            return TokenUsage::default();
        };
        let Some(response) = record.get("response") else {
            return TokenUsage::default();
        };
        if response
            .get("status")
            .and_then(|v| v.as_u64())
            .is_some_and(|s| !(200..300).contains(&s))
        {
            continue;
        }
        let url = record
            .pointer("/request/url")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if !url.is_empty()
            && ![
                "/messages",
                "/chat/completions",
                "/responses",
                "/api/chat",
                "/api/generate",
            ]
            .iter()
            .any(|suffix| url.split('?').next().unwrap_or(url).ends_with(suffix))
        {
            continue;
        }
        let Some(body) = response.get("body").and_then(|v| v.as_str()) else {
            return TokenUsage::default();
        };
        let mut call = TokenUsage::default();
        let mut cache_write = None;
        let mut found = false;
        let mut take = |event: &serde_json::Value| {
            let Some(value) = event
                .get("usage")
                .or_else(|| event.pointer("/message/usage"))
                .or_else(|| event.pointer("/response/usage"))
                .or_else(|| event.get("usageMetadata"))
            else {
                return;
            };
            let observed = crate::usage::usage_from_json(value);
            if observed.any() {
                found = true;
                let max = |a: Option<u64>, b: Option<u64>| match (a, b) {
                    (Some(a), Some(b)) => Some(a.max(b)),
                    (a, b) => a.or(b),
                };
                call.input_tokens = max(call.input_tokens, observed.input_tokens);
                call.output_tokens = max(call.output_tokens, observed.output_tokens);
                call.cached_tokens = max(call.cached_tokens, observed.cached_tokens);
                call.total_tokens = max(call.total_tokens, observed.total_tokens);
                cache_write = max(
                    cache_write,
                    value
                        .get("cache_creation_input_tokens")
                        .and_then(|v| v.as_u64()),
                );
            }
        };
        let mut terminated = true;
        if let Ok(event) = serde_json::from_str::<serde_json::Value>(body) {
            take(&event);
        } else {
            terminated = false;
            for line in body.lines() {
                if let Some(data) = line.trim_start().strip_prefix("data:") {
                    if data.trim() == "[DONE]" {
                        terminated = true;
                    }
                    if let Ok(event) = serde_json::from_str::<serde_json::Value>(data.trim()) {
                        if matches!(
                            event.get("type").and_then(|v| v.as_str()),
                            Some("message_stop" | "response.completed" | "response.incomplete")
                        ) {
                            terminated = true;
                        }
                        take(&event);
                    }
                }
            }
        }
        if !terminated
            || !found
            || call.input_tokens.is_none()
            || call.output_tokens.is_none()
            || (call.input_tokens == Some(0) && call.output_tokens == Some(0))
        {
            return TokenUsage::default();
        }
        if url.split('?').next().unwrap_or(url).ends_with("/messages") && !input_includes_cache {
            call.input_tokens = call.input_tokens.map(|n| {
                n.saturating_add(call.cached_tokens.unwrap_or(0))
                    .saturating_add(cache_write.unwrap_or(0))
            });
        }
        cache_complete &= call.cached_tokens.is_some();
        sum = sum.add(call.with_derived_total());
    }
    if !cache_complete {
        sum.cached_tokens = None;
    }
    sum.with_derived_total()
}

#[cfg(test)]
mod streaming_tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn large_recording_is_streamed_and_truncation_remains_unknown() {
        let path = std::env::temp_dir().join(format!("asb-large-usage-{}", std::process::id()));
        let mut file = std::fs::File::create(&path).unwrap();
        let record = serde_json::json!({
            "request": {"url": "/responses", "padding": "x".repeat(1024 * 1024)},
            "response": {"status": 200, "body": "{\"usage\":{\"input_tokens\":3,\"output_tokens\":2,\"total_tokens\":5}}"}
        }).to_string();
        for _ in 0..17 {
            writeln!(file, "{record}").unwrap();
        }
        file.flush().unwrap();
        let usage = from_file(&path, false);
        assert_eq!(usage.input_tokens, Some(51));
        assert_eq!(usage.output_tokens, Some(34));
        assert_eq!(usage.total_tokens, Some(85));
        let reported = crate::usage::from_sources_with_recorder(
            "openrouter",
            "codex",
            "",
            "",
            Some(&path),
            crate::proxy::default_recorder(),
        );
        assert_eq!(reported.total_tokens, Some(85));
        assert_eq!(reported.source.as_deref(), Some("cassette"));
        writeln!(file, "{{\"response\":").unwrap();
        file.flush().unwrap();
        assert!(!from_file(&path, false).any());
        std::fs::remove_file(path).unwrap();
    }
}
