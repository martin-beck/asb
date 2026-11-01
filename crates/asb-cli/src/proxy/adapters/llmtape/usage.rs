//! Replay-cassette usage evidence.
//!
//! The proxy owns the request/response wire records. This module converts only
//! complete successful cassette responses into normalized usage observations.

use crate::usage::TokenUsage;

/// A cassette has one response per request. SSE usage snapshots within one
/// response are cumulative, not separate calls; only sum across responses.
pub(crate) fn from_cassette(text: &str, input_includes_cache: bool) -> TokenUsage {
    let mut sum = TokenUsage::default();
    let mut cache_complete = true;
    for line in text.lines() {
        let Ok(record) = serde_json::from_str::<serde_json::Value>(line) else {
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
