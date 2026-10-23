//! Bounded, provider-neutral extraction of structured agent tool-call events.

use serde::Serialize;

const MAX_EVENTS: usize = 16_384;

/// A tool invocation as reported by the agent's machine-readable transcript.
/// Arguments are intentionally not copied: they may contain task material or
/// credentials, while the call identity is sufficient for measurement.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct ToolCall {
    pub ordinal: usize,
    pub source: &'static str,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub arguments_present: bool,
}

pub(crate) fn observed_tool_calls(stdout: &str, stderr: &str) -> Vec<ToolCall> {
    let mut calls = Vec::new();
    for (source, text) in [("stdout", stdout), ("stderr", stderr)] {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(text) {
            collect(&value, source, &mut calls);
        } else {
            for line in text.lines().filter(|line| !line.trim().is_empty()) {
                if let Ok(value) = serde_json::from_str::<serde_json::Value>(line) {
                    collect(&value, source, &mut calls);
                }
            }
        }
    }
    calls
}

fn collect(value: &serde_json::Value, source: &'static str, calls: &mut Vec<ToolCall>) {
    if calls.len() >= MAX_EVENTS {
        return;
    }
    match value {
        serde_json::Value::Array(values) => {
            for value in values {
                collect(value, source, calls);
            }
        }
        serde_json::Value::Object(object) => {
            if let Some(values) = object
                .get("tool_calls")
                .and_then(serde_json::Value::as_array)
            {
                for value in values {
                    record(value, source, "tool_call", calls);
                }
                return;
            }
            if let Some(value) = object.get("function_call") {
                record(value, source, "function_call", calls);
                return;
            }
            let kind = object
                .get("type")
                .and_then(serde_json::Value::as_str)
                .map(normalize);
            if kind.as_deref().is_some_and(is_tool_call_kind) {
                record(value, source, kind.as_deref().unwrap_or("tool_call"), calls);
                return;
            }
            for value in object.values() {
                collect(value, source, calls);
            }
        }
        _ => {}
    }
}

fn record(
    value: &serde_json::Value,
    source: &'static str,
    fallback: &str,
    calls: &mut Vec<ToolCall>,
) {
    if calls.len() >= MAX_EVENTS {
        return;
    }
    let object = value.as_object();
    let function = object.and_then(|object| object.get("function"));
    let name = object
        .and_then(|object| object.get("name").or_else(|| object.get("tool_name")))
        .or_else(|| function.and_then(|value| value.get("name")))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    let id = object
        .and_then(|object| object.get("call_id").or_else(|| object.get("id")))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    let arguments_present = object.is_some_and(|object| {
        ["arguments", "input", "parameters"]
            .iter()
            .any(|key| object.contains_key(*key))
            || function.is_some_and(|value| value.get("arguments").is_some())
    });
    let kind = object
        .and_then(|object| object.get("type"))
        .and_then(serde_json::Value::as_str)
        .map(normalize)
        .filter(|kind| is_tool_call_kind(kind))
        .unwrap_or_else(|| fallback.to_owned());
    calls.push(ToolCall {
        ordinal: calls.len() + 1,
        source,
        kind,
        name,
        id,
        arguments_present,
    });
}

fn normalize(kind: &str) -> String {
    kind.to_ascii_lowercase().replace(['-', '.'], "_")
}

fn is_tool_call_kind(kind: &str) -> bool {
    matches!(kind, "tool_call" | "tool_use" | "function_call")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_tool_calls_from_structured_events_without_retaining_arguments() {
        let calls = observed_tool_calls(
            r#"{"type":"response","items":[{"type":"function_call","name":"shell","call_id":"call-1","arguments":"secret"},{"tool_calls":[{"function":{"name":"read_file","arguments":"private"}}]}]}"#,
            "",
        );
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name.as_deref(), Some("shell"));
        assert_eq!(calls[0].id.as_deref(), Some("call-1"));
        assert!(calls.iter().all(|call| call.arguments_present));
        assert!(!serde_json::to_string(&calls).unwrap().contains("secret"));
        assert!(!serde_json::to_string(&calls).unwrap().contains("private"));
    }
}
