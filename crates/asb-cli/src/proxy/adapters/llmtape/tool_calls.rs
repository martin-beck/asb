//! llmtape provider-response tool-call extraction.
use crate::proxy::tool_calls::ToolCall;
use serde_json::Value;
use std::collections::HashSet;
use std::path::Path;
const MAX_EVENTS: usize = 16_384;

pub(crate) fn from_cassette(path: &Path) -> Option<Vec<ToolCall>> {
    let text = crate::models::read_replay_cassette(path).ok()?;
    let interactions = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(serde_json::from_str::<Value>)
        .collect::<Result<Vec<_>, _>>()
        .ok()?;
    let mut calls = Vec::new();
    for (index, interaction) in interactions.iter().enumerate() {
        let response = interaction
            .get("response")
            .and_then(|value| value.get("body"))
            .and_then(Value::as_str);
        let Some(response) = response else { continue };
        let timestamp = interaction
            .pointer("/timing/response_observed_unix_ms")
            .and_then(Value::as_u64)
            .map(u128::from);
        collect_response(response, timestamp, &interactions[index + 1..], &mut calls);
        if calls.len() >= MAX_EVENTS {
            break;
        }
    }
    Some(calls)
}

fn collect_response(
    response: &str,
    timestamp: Option<u128>,
    following: &[Value],
    calls: &mut Vec<ToolCall>,
) {
    // Call identifiers are only unique within a provider response.  llmtape
    // deliberately normalizes each response independently, so a cassette can
    // legitimately contain `<asb-call-1>` in many distinct turns.  Keep the
    // de-duplication boundary here: it removes repeated SSE fragments without
    // collapsing real calls from later responses.
    let mut response_call_ids = HashSet::new();
    if let Ok(value) = serde_json::from_str::<Value>(response) {
        collect(
            &value,
            timestamp,
            following,
            false,
            &mut response_call_ids,
            calls,
        );
        return;
    }
    // Streaming providers put JSON values in SSE `data:` lines.  The same
    // response may contain partial deltas, so only complete tool-call shapes
    // are retained and duplicate ids are suppressed below.
    for line in response
        .lines()
        .filter_map(|line| line.strip_prefix("data:").map(str::trim))
    {
        if line != "[DONE]"
            && let Ok(value) = serde_json::from_str::<Value>(line)
        {
            collect(
                &value,
                timestamp,
                following,
                true,
                &mut response_call_ids,
                calls,
            );
        }
    }
}

fn collect(
    value: &Value,
    timestamp: Option<u128>,
    following: &[Value],
    streamed: bool,
    response_call_ids: &mut HashSet<String>,
    calls: &mut Vec<ToolCall>,
) {
    if calls.len() >= MAX_EVENTS {
        return;
    }
    match value {
        Value::Array(values) => {
            for value in values {
                collect(
                    value,
                    timestamp,
                    following,
                    streamed,
                    response_call_ids,
                    calls,
                );
            }
        }
        Value::Object(object) => {
            if let Some(values) = object.get("tool_calls").and_then(Value::as_array) {
                for value in values {
                    record(
                        value,
                        "tool_call",
                        timestamp,
                        following,
                        streamed,
                        response_call_ids,
                        calls,
                    );
                }
                return;
            }
            if let Some(value) = object.get("function_call") {
                record(
                    value,
                    "function_call",
                    timestamp,
                    following,
                    streamed,
                    response_call_ids,
                    calls,
                );
                return;
            }
            let kind = object.get("type").and_then(Value::as_str).map(normalize);
            if kind.as_deref().is_some_and(is_tool_kind) {
                record(
                    value,
                    kind.as_deref().unwrap(),
                    timestamp,
                    following,
                    streamed,
                    response_call_ids,
                    calls,
                );
                return;
            }
            for value in object.values() {
                collect(
                    value,
                    timestamp,
                    following,
                    streamed,
                    response_call_ids,
                    calls,
                );
            }
        }
        _ => {}
    }
}

fn record(
    value: &Value,
    fallback: &str,
    timestamp: Option<u128>,
    following: &[Value],
    streamed: bool,
    response_call_ids: &mut HashSet<String>,
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
        .and_then(Value::as_str)
        .map(str::to_owned);
    let id = object
        .and_then(|object| object.get("call_id").or_else(|| object.get("id")))
        .and_then(Value::as_str)
        .map(str::to_owned);
    // A delta that carries only `type: tool_call` is not an executable tool
    // invocation. In particular, never turn streaming framing fragments into
    // countable measurements. Providers which omit an id in a complete JSON
    // response are still represented, provided they name the operation.
    let Some(name) = name else { return };
    if streamed && id.is_none() {
        return;
    }
    // SSE deltas often repeat the same id. A complete response must count a
    // provider call once, not once per chunk.
    if id
        .as_ref()
        .is_some_and(|id| !response_call_ids.insert(id.clone()))
    {
        return;
    }
    let arguments = object
        .and_then(|object| {
            object
                .get("arguments")
                .or_else(|| object.get("input"))
                .or_else(|| object.get("parameters"))
        })
        .or_else(|| function.and_then(|value| value.get("arguments")));
    let arguments_present = arguments.is_some();
    let duration = id
        .as_deref()
        .and_then(|id| execution_duration(id, timestamp, following));
    let mut classes = classify(Some(&name), arguments);
    classes.sort();
    classes.dedup();
    calls.push(ToolCall {
        ordinal: calls.len() + 1,
        source: "llmtape.provider_response",
        kind: object
            .and_then(|object| object.get("type"))
            .and_then(Value::as_str)
            .map(normalize)
            .filter(|kind| is_tool_kind(kind))
            .unwrap_or_else(|| fallback.into()),
        name: Some(name),
        id,
        arguments_present,
        response_observed_unix_ms: timestamp,
        execution_duration_ms: duration,
        classes,
    });
}

/// Return timing only for the request that actually carries this call's result.
/// An unrelated provider exchange may occur between a call and its result.
fn execution_duration(id: &str, timestamp: Option<u128>, following: &[Value]) -> Option<u128> {
    let start = timestamp?;
    following.iter().find_map(|interaction| {
        let request = interaction
            .pointer("/request/body")
            .and_then(Value::as_str)?;
        request_references_call(request, id).then_some(())?;
        let end = interaction
            .pointer("/timing/request_received_unix_ms")
            .and_then(Value::as_u64)
            .map(u128::from)?;
        end.checked_sub(start)
    })
}

fn request_references_call(request: &str, id: &str) -> bool {
    serde_json::from_str::<Value>(request)
        .ok()
        .is_some_and(|value| value_references_call(&value, id))
}

fn value_references_call(value: &Value, id: &str) -> bool {
    match value {
        Value::Array(values) => values.iter().any(|value| value_references_call(value, id)),
        Value::Object(values) => values.iter().any(|(key, value)| {
            matches!(key.as_str(), "tool_call_id" | "call_id" | "tool_use_id")
                && value.as_str() == Some(id)
                || value_references_call(value, id)
        }),
        _ => false,
    }
}

fn classify(name: Option<&str>, arguments: Option<&Value>) -> Vec<String> {
    let name = name.unwrap_or_default().to_ascii_lowercase();
    let args = arguments
        .map(Value::to_string)
        .unwrap_or_default()
        .to_ascii_lowercase();
    let mut classes = Vec::new();
    {
        let mut add = |class: &str| classes.push(class.to_owned());
        if name.contains("read")
            || name.contains("view")
            || name.contains("cat")
            || name.contains("list")
        {
            add("filesystem.read");
        }
        if name.contains("write")
            || name.contains("edit")
            || name.contains("patch")
            || name.contains("create")
        {
            add("filesystem.write");
        }
        if name.contains("delete") || name.contains("remove") || name.contains("unlink") {
            add("filesystem.delete");
        }
        if name.contains("move") || name.contains("rename") || name.contains("copy") {
            add("filesystem.move");
        }
        if name.contains("exec")
            || name.contains("shell")
            || name.contains("command")
            || name.contains("terminal")
            || matches!(
                name.as_str(),
                "bash" | "sh" | "zsh" | "fish" | "cmd" | "powershell"
            )
        {
            add("process.execute");
            add("shell.command");
        }
        if name.contains("search") || name.contains("grep") || name.contains("find") {
            add("search.local");
        }
        if name.contains("web")
            || name.contains("browse")
            || name.contains("http")
            || args.contains("http://")
            || args.contains("https://")
        {
            add("network.api");
        }
        if name.contains("download")
            || name.contains("fetch")
            || name.contains("curl")
            || name.contains("wget")
        {
            add("network.download");
        }
        if name.contains("upload") {
            add("network.upload");
        }
        if name.contains("git") || name.contains("commit") || name.contains("diff") {
            add("source_control.read");
        }
        if name.contains("install") || name.contains("package") {
            add("package.install");
        }
        if name.contains("build") || name.contains("compile") {
            add("build");
        }
        if name.contains("test") {
            add("test");
        }
        if name.contains("sql") || name.contains("database") || name.contains("query") {
            add("database.query");
        }
        if name.contains("message") || name.contains("notify") {
            add("communication.message");
        }
        if name.contains("delegate") || name.contains("agent") {
            add("agent.delegate");
        }
    }
    if classes.is_empty() {
        classes.push("other".into());
    }
    classes
}

fn normalize(value: &str) -> String {
    value.to_ascii_lowercase().replace(['-', '.'], "_")
}
fn is_tool_kind(value: &str) -> bool {
    matches!(value, "tool_call" | "tool_use" | "function_call")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn ignores_tool_calls_from_a_symlinked_cassette() {
        use std::os::unix::fs::symlink;

        let root = std::env::temp_dir().join(format!(
            "asb-tool-call-symlink-{}",
            crate::tools::runtime::unique_id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let target = root.join("target.jsonl");
        let link = root.join("cassette.jsonl");
        std::fs::write(
            &target,
            r#"{"request":{"body":"{}"},"response":{"body":"{\"type\":\"tool_call\",\"name\":\"bash\"}"}}"#,
        )
        .unwrap();
        symlink(&target, &link).unwrap();
        assert!(from_cassette(&link).is_none());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn invalid_cassette_does_not_claim_zero_observed_calls() {
        let path = std::env::temp_dir().join(format!(
            "asb-invalid-tool-calls-{}",
            crate::tools::runtime::unique_id()
        ));
        std::fs::write(&path, "not-json\n").unwrap();
        assert!(from_cassette(&path).is_none());
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn extracts_only_provider_response_calls_with_timestamp_duration_and_classes() {
        let root = std::env::temp_dir().join(format!(
            "asb-tool-calls-{}",
            crate::tools::runtime::unique_id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("tape.jsonl");
        std::fs::write(&path, concat!(
            r#"{"request":{"body":"{}"},"response":{"body":"{\"choices\":[{\"message\":{\"tool_calls\":[{\"id\":\"call-1\",\"type\":\"function\",\"function\":{\"name\":\"read_file\",\"arguments\":\"secret\"}}]}}]}"},"timing":{"request_received_unix_ms":100,"response_observed_unix_ms":120}}"#, "\n",
            r#"{"request":{"body":"{\"tool_call_id\":\"call-1\"}"},"response":{"body":"{}"},"timing":{"request_received_unix_ms":170,"response_observed_unix_ms":180}}"#, "\n")).unwrap();
        let calls = from_cassette(&path).unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].response_observed_unix_ms, Some(120));
        assert_eq!(calls[0].execution_duration_ms, Some(50));
        assert!(
            calls[0]
                .classes
                .iter()
                .any(|class| class == "filesystem.read")
        );
        assert!(!serde_json::to_string(&calls).unwrap().contains("secret"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn ignores_anonymous_streaming_fragments_and_waits_for_the_matching_result() {
        let root = std::env::temp_dir().join(format!(
            "asb-tool-call-stream-{}",
            crate::tools::runtime::unique_id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("tape.jsonl");
        std::fs::write(&path, concat!(
            r#"{"request":{"body":"{}"},"response":{"body":"data: {\"type\":\"tool_call\"}\ndata: {\"type\":\"tool_call\",\"id\":\"call-1\",\"name\":\"bash\",\"arguments\":\"{}\"}\n\n"},"timing":{"response_observed_unix_ms":120}}"#, "\n",
            r#"{"request":{"body":"{\"unrelated\":true}"},"response":{"body":"{}"},"timing":{"request_received_unix_ms":140,"response_observed_unix_ms":145}}"#, "\n",
            r#"{"request":{"body":"{\"tool_call_id\":\"call-1\"}"},"response":{"body":"{}"},"timing":{"request_received_unix_ms":170,"response_observed_unix_ms":180}}"#, "\n",
            r#"{"request":{"body":"{}"},"response":{"body":"{\"type\":\"tool_call\",\"name\":\"read\",\"arguments\":\"{}\"}"},"timing":{"response_observed_unix_ms":200}}"#, "\n",
        )).unwrap();

        let calls = from_cassette(&path).unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name.as_deref(), Some("bash"));
        assert_eq!(calls[0].execution_duration_ms, Some(50));
        assert!(calls[0].classes.contains(&"process.execute".into()));
        assert!(calls[0].classes.contains(&"shell.command".into()));
        assert_eq!(calls[1].name.as_deref(), Some("read"));
        assert_eq!(calls[1].id, None);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn counts_reused_normalized_ids_in_distinct_provider_responses() {
        let root = std::env::temp_dir().join(format!(
            "asb-tool-call-response-scope-{}",
            crate::tools::runtime::unique_id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("tape.jsonl");
        std::fs::write(&path, concat!(
            r#"{"request":{"body":"{}"},"response":{"body":"{\"type\":\"tool_call\",\"id\":\"<asb-call-1>\",\"name\":\"read_file\",\"arguments\":\"{}\"}"},"timing":{"response_observed_unix_ms":120}}"#, "\n",
            r#"{"request":{"body":"{}"},"response":{"body":"{\"type\":\"tool_call\",\"id\":\"<asb-call-1>\",\"name\":\"write_file\",\"arguments\":\"{}\"}"},"timing":{"response_observed_unix_ms":140}}"#, "\n",
        )).unwrap();

        let calls = from_cassette(&path).unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name.as_deref(), Some("read_file"));
        assert_eq!(calls[1].name.as_deref(), Some("write_file"));
        std::fs::remove_dir_all(root).unwrap();
    }
}
