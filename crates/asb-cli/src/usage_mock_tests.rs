//! Token accounting contracts using synthetic output and cassette data only.
//! No test starts an agent, proxy, provider, or network connection.
use super::{TokenUsage, from_sources as token_usage_from_sources};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_FILE: AtomicU64 = AtomicU64::new(0);

#[test]
fn mock_fixture_matrix_covers_every_registered_agent() {
    use std::collections::BTreeSet;
    let covered = BTreeSet::from([
        "claude-code",
        "cline",
        "codex",
        "continue",
        "goose",
        "hermes",
        "mini-swe",
        "opencode",
        "openhands",
        "openjiuwen",
        "pi",
        "qwen-code",
        "workbuddy",
    ]);
    let registered = crate::agents::names().collect::<BTreeSet<_>>();
    assert_eq!(
        covered, registered,
        "add token-usage fixtures for new agents"
    );
}

struct Fixture(PathBuf);
impl Fixture {
    fn new(contents: &str) -> Self {
        use std::io::Write;
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/offline-token-usage-fixtures");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!(
            "usage-{}-{}-{}.jsonl",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT_FILE.fetch_add(1, Ordering::Relaxed)
        ));
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        file.write_all(contents.as_bytes()).unwrap();
        Self(path)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn assert_usage(usage: TokenUsage, input: u64, output: u64, cached: Option<u64>) {
    assert_eq!(usage.input_tokens, Some(input), "input: {usage:?}");
    assert_eq!(usage.output_tokens, Some(output), "output: {usage:?}");
    assert_eq!(usage.cached_tokens, cached, "cached: {usage:?}");
    assert_eq!(usage.total_tokens, Some(input + output), "total: {usage:?}");
}

fn chat_cassette(calls: usize) -> Fixture {
    let body = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"answer\"}}]}\n\n",
        "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":3,\"prompt_tokens_details\":{\"cached_tokens\":4}}}\n\n",
        "data: [DONE]\n\n",
    );
    let record = serde_json::json!({
        "request":{"url":"/v1/chat/completions"},
        "response":{"status":200,"body":body}
    });
    Fixture::new(
        &(0..calls)
            .map(|_| format!("{record}\n"))
            .collect::<String>(),
    )
}

#[test]
fn unregistered_agent_output_is_not_usage_but_cassette_is() {
    let output = r#"{"usage":{"input_tokens":999,"output_tokens":888}}"#;
    let unknown = token_usage_from_sources("openrouter", "unregistered-agent", output, "", None);
    assert!(!unknown.any());
    let cassette = chat_cassette(2);
    let usage = token_usage_from_sources(
        "openrouter",
        "unregistered-agent",
        output,
        "",
        Some(&cassette.0),
    );
    assert_eq!(usage.source.as_deref(), Some("cassette"));
    assert_usage(usage, 20, 6, Some(8));
}

#[test]
fn hermes_uses_cassette_not_token_shaped_final_answer() {
    let cassette = chat_cassette(2);
    let answer = r#"{"usage":{"input_tokens":999,"output_tokens":888}}"#;
    let usage = token_usage_from_sources("openrouter", "hermes", answer, "", Some(&cassette.0));
    assert_eq!(usage.source.as_deref(), Some("cassette"));
    assert_usage(usage, 20, 6, Some(8));
}

#[test]
fn qwen_code_uses_the_final_cumulative_summary_once() {
    let output = concat!(
        "{\"type\":\"message\",\"usage\":{\"input_tokens\":0,\"output_tokens\":0}}\n",
        "{\"type\":\"result\",\"usage\":{\"input_tokens\":20,\"output_tokens\":6,\"cached_tokens\":8,\"total_tokens\":26}}\n",
    );
    assert_usage(
        token_usage_from_sources("openrouter", "qwen-code", output, "", None),
        20,
        6,
        Some(8),
    );
}

#[test]
fn openjiuwen_sums_calls_not_context_estimates() {
    let output = concat!(
        "{\"type\":\"context_usage\",\"payload\":{\"input_tokens\":9999}}\n",
        "{\"type\":\"llm_usage\",\"payload\":{\"usage_metadata\":{\"input_tokens\":16,\"output_tokens\":3,\"cache_read_tokens\":4,\"cache_creation_input_tokens\":2}}}\n",
        "{\"type\":\"llm_usage\",\"payload\":{\"usage_metadata\":{\"input_tokens\":16,\"output_tokens\":3,\"cache_read_tokens\":4,\"cache_creation_input_tokens\":2}}}\n",
    );
    assert_usage(
        token_usage_from_sources("anthropic", "openjiuwen", output, "", None),
        32,
        6,
        Some(8),
    );
    // The mock bridge includes cached tokens in its wire input already.
    assert_usage(
        token_usage_from_sources("cli2key", "openjiuwen", output, "", None),
        20,
        6,
        Some(8),
    );
}

#[test]
fn codex_includes_reported_cached_input_without_double_counting() {
    let output = concat!(
        "{\"type\":\"thread.started\"}\n",
        "{\"type\":\"turn.completed\",\"usage\":{\"input_tokens\":10,\"cached_input_tokens\":4,\"output_tokens\":3}}\n",
    );
    assert_usage(
        token_usage_from_sources("openrouter", "codex", output, "", None),
        10,
        3,
        Some(4),
    );
}

#[test]
fn claude_code_single_result_is_not_read_twice() {
    let output = "{\"type\":\"result\",\"usage\":{\"input_tokens\":10,\"output_tokens\":3,\"cache_read_input_tokens\":0},\"modelUsage\":{\"mock\":{\"inputTokens\":10,\"outputTokens\":3}}}\n";
    assert_usage(
        token_usage_from_sources("anthropic", "claude-code", output, "", None),
        10,
        3,
        Some(0),
    );
}

#[test]
fn claude_code_incomplete_result_uses_cassette_not_answer() {
    let cassette = chat_cassette(2);
    let output = serde_json::json!({"type":"result","result":"{\"usage\":{\"input_tokens\":999,\"output_tokens\":888}}"}).to_string();
    let usage =
        token_usage_from_sources("openrouter", "claude-code", &output, "", Some(&cassette.0));
    assert_eq!(usage.source.as_deref(), Some("cassette"));
    assert_usage(usage, 20, 6, Some(8));
}

#[test]
fn claude_cache_components_follow_provider_wire_semantics() {
    let output = r#"{"type":"result","usage":{"input_tokens":10,"output_tokens":3,"cache_read_input_tokens":4,"cache_creation_input_tokens":2}}"#;
    assert_usage(
        token_usage_from_sources("anthropic", "claude-code", output, "", None),
        16,
        3,
        Some(4),
    );
    assert_usage(
        token_usage_from_sources("cli2key", "claude-code", output, "", None),
        10,
        3,
        Some(4),
    );
}

#[test]
fn cline_repeated_current_and_final_snapshots_count_once() {
    let output = concat!(
        "{\"type\":\"agent_event\",\"event\":{\"type\":\"usage\",\"inputTokens\":10,\"outputTokens\":3}}\n",
        "{\"type\":\"agent_event\",\"event\":{\"type\":\"done\",\"usage\":{\"inputTokens\":10,\"outputTokens\":3}}}\n",
        "{\"type\":\"run_result\",\"usage\":{\"inputTokens\":10,\"outputTokens\":3},\"aggregateUsage\":{\"inputTokens\":10,\"outputTokens\":3}}\n",
    );
    assert_usage(
        token_usage_from_sources("openrouter", "cline", output, "", None),
        10,
        3,
        None,
    );
}

#[test]
fn cline_incomplete_final_result_defers_to_cassette() {
    let output = concat!(
        "{\"type\":\"agent_event\",\"event\":{\"type\":\"usage\",\"inputTokens\":999,\"outputTokens\":888}}\n",
        "{\"type\":\"run_result\",\"iterations\":1}\n",
    );
    let cassette = chat_cassette(2);
    let usage = token_usage_from_sources("openrouter", "cline", output, "", Some(&cassette.0));
    assert_eq!(usage.source.as_deref(), Some("cassette"));
    assert_usage(usage, 20, 6, Some(8));
}

#[test]
fn pi_cumulative_events_count_once_and_include_cached_input() {
    let output = concat!(
        "{\"type\":\"message_update\",\"usage\":{\"input\":6,\"output\":3,\"cacheRead\":4,\"totalTokens\":13}}\n",
        "{\"type\":\"message_end\",\"usage\":{\"input\":6,\"output\":3,\"cacheRead\":4,\"totalTokens\":13}}\n",
        "{\"type\":\"turn_end\",\"usage\":{\"input\":6,\"output\":3,\"cacheRead\":4,\"totalTokens\":13}}\n",
        "{\"type\":\"agent_end\",\"usage\":{\"input\":6,\"output\":3,\"cacheRead\":4,\"totalTokens\":13}}\n",
    );
    assert_usage(
        token_usage_from_sources("openrouter", "pi", output, "", None),
        10,
        3,
        Some(4),
    );
}

#[test]
fn pi_counts_completed_turns_and_not_intermediate_snapshots() {
    let output = concat!(
        "{\"type\":\"turn_end\",\"message\":{\"usage\":{\"input\":6,\"output\":3,\"cacheRead\":4,\"cacheWrite\":2}}}\n",
        "{\"type\":\"message_update\",\"usage\":{\"input\":99,\"output\":99}}\n",
        "{\"type\":\"turn_end\",\"message\":{\"usage\":{\"input\":7,\"output\":2,\"cacheRead\":3}}}\n",
        "{\"type\":\"agent_end\"}\n",
    );
    assert_usage(
        token_usage_from_sources("openrouter", "pi", output, "", None),
        22,
        5,
        Some(7),
    );
}

#[test]
fn incomplete_completed_turn_does_not_yield_partial_totals() {
    let output = concat!(
        "{\"type\":\"turn_end\",\"usage\":{\"input\":10,\"output\":3}}\n",
        "{\"type\":\"turn_end\",\"usage\":{\"input\":10}}\n",
    );
    assert_eq!(
        token_usage_from_sources("openrouter", "pi", output, "", None).total_tokens,
        None
    );
}

#[test]
fn pi_incomplete_turns_defer_to_complete_cassette_usage() {
    let output = concat!(
        "{\"type\":\"turn_end\",\"usage\":{\"input\":10,\"output\":3,\"cacheRead\":2}}\n",
        "{\"type\":\"turn_end\",\"usage\":{\"input\":10}}\n",
    );
    let without_cassette = token_usage_from_sources("openrouter", "pi", output, "", None);
    assert!(without_cassette.input_tokens.is_none());
    assert!(without_cassette.output_tokens.is_none());
    assert!(without_cassette.total_tokens.is_none());
    let cassette = chat_cassette(2);
    let usage = token_usage_from_sources("openrouter", "pi", output, "", Some(&cassette.0));
    assert_eq!(usage.source.as_deref(), Some("cassette"));
    assert_usage(usage, 20, 6, Some(8));
}

#[test]
fn workbuddy_pretty_printed_result_counts_one_usage_record() {
    let output = "[\n  {\n    \"type\": \"message\",\n    \"usage\": {\"inputTokens\":10,\"outputTokens\":3,\"totalTokens\":13}\n  },\n  {\n    \"type\": \"result\",\n    \"usage\": {\"input_tokens\":10,\"output_tokens\":3,\"cache_read_input_tokens\":0}\n  }\n]\n";
    assert_usage(
        token_usage_from_sources("openrouter", "workbuddy", output, "", None),
        10,
        3,
        Some(0),
    );
}

#[test]
fn workbuddy_incomplete_result_uses_complete_cassette() {
    let cassette = chat_cassette(2);
    let output = serde_json::json!({"type":"result", "result":"{\"usage\":{\"input_tokens\":999,\"output_tokens\":888}}"}).to_string();
    let usage = token_usage_from_sources("openrouter", "workbuddy", &output, "", Some(&cassette.0));
    assert_eq!(usage.source.as_deref(), Some("cassette"));
    assert_usage(usage, 20, 6, Some(8));
}

#[test]
fn mini_swe_trajectory_counts_each_response_once() {
    let trajectory = Fixture::new(concat!(
        "{\n  \"info\":{\"model_stats\":{\"api_calls\":3}},\n  \"messages\":[\n",
        "    {\"role\":\"assistant\",\"extra\":{\"response\":{\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":3,\"prompt_tokens_details\":{\"cached_tokens\":4}}}}},\n",
        "    {\"role\":\"assistant\",\"extra\":{\"response\":{\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":3,\"prompt_tokens_details\":{\"cached_tokens\":4}}}}},\n",
        "    {\"role\":\"assistant\",\"extra\":{\"response\":{\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":3,\"prompt_tokens_details\":{\"cached_tokens\":4}}}}}\n",
        "  ],\n  \"trajectory_format\":\"mini-swe-agent-1.1\"\n}\n",
    ));
    let workspace = trajectory.0.with_extension("workspace");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::copy(&trajectory.0, workspace.join(".asb-mini-swe.traj.json")).unwrap();
    let artifacts =
        crate::agents::find("mini-swe")
            .unwrap()
            .collect(&crate::agents::CollectionContext {
                provider_capture: None,
                frozen: &crate::agents::FrozenArtifacts::for_test(
                    "trajectory.json",
                    serde_json::from_slice(&std::fs::read(&trajectory.0).unwrap()).unwrap(),
                ),
                run_dir: &workspace,
                interface: None,
                anthropic_input_includes_cache: false,
                mcp_servers: &[],
                mcp_tape: None,
            });
    assert_usage(artifacts.token_usage.unwrap(), 30, 9, Some(12));
    std::fs::remove_file(workspace.join(".asb-mini-swe.traj.json")).unwrap();
    std::fs::remove_dir(workspace).unwrap();
}

#[test]
fn mini_swe_uses_complete_cassette_instead_of_untrusted_stdout() {
    let cassette = chat_cassette(2);
    let output = r#"{"usage":{"input_tokens":999,"output_tokens":888}}"#;
    let usage = token_usage_from_sources("openrouter", "mini-swe", output, "", Some(&cassette.0));
    assert_eq!(usage.source.as_deref(), Some("cassette"));
    assert_usage(usage, 20, 6, Some(8));
}

#[test]
fn goose_recording_includes_the_separate_title_call() {
    let cassette = chat_cassette(2);
    let output = "{\"metadata\":{\"input_tokens\":10,\"output_tokens\":3,\"cache_read_input_tokens\":4,\"total_tokens\":13}}";
    assert_usage(
        token_usage_from_sources("openrouter", "goose", output, "", Some(&cassette.0)),
        20,
        6,
        Some(8),
    );
}

#[test]
fn goose_native_summary_uses_accumulated_totals_and_includes_cache() {
    let output = "Goose banner\n{\"messages\":[{\"role\":\"assistant\",\"metadata\":{\"usage\":{\"inputTokens\":5,\"outputTokens\":2}}}],\"metadata\":{\"status\":\"completed\",\"input_tokens\":10,\"output_tokens\":3,\"cache_read_input_tokens\":4,\"cache_write_input_tokens\":2,\"total_tokens\":13}}";
    assert_usage(
        token_usage_from_sources("anthropic", "goose", output, "", None),
        10,
        3,
        Some(4),
    );
}

#[test]
fn opencode_recording_counts_title_and_cached_benchmark_input() {
    let cassette = chat_cassette(2);
    let output = concat!(
        "{\"type\":\"step_start\"}\n",
        "{\"type\":\"step_finish\",\"part\":{\"tokens\":{\"input\":6,\"output\":3,\"total\":13,\"cache\":{\"read\":4}}}}\n",
    );
    assert_usage(
        token_usage_from_sources("openrouter", "opencode", output, "", Some(&cassette.0)),
        20,
        6,
        Some(8),
    );
    assert_usage(
        token_usage_from_sources("openrouter", "opencode", output, "", None),
        10,
        3,
        Some(4),
    );
}

#[test]
fn continue_and_openhands_use_recorded_mock_usage_when_output_omits_it() {
    let cassette = chat_cassette(1);
    for agent in ["continue", "openhands"] {
        let output = r#"{"response":"fixture answer","status":"success"}"#;
        let unknown = token_usage_from_sources("openrouter", agent, output, "", None);
        assert_eq!(unknown.total_tokens, None, "{agent}: do not invent usage");
        assert_usage(
            token_usage_from_sources("openrouter", agent, output, "", Some(&cassette.0)),
            10,
            3,
            Some(4),
        );
    }
}

#[test]
fn continue_prefers_cassette_to_usage_shaped_json_answer() {
    let cassette = chat_cassette(2);
    let answer = r#"{"usage":{"input_tokens":999,"output_tokens":888}}"#;
    let usage = token_usage_from_sources("openrouter", "continue", answer, "", Some(&cassette.0));
    assert_eq!(usage.source.as_deref(), Some("cassette"));
    assert_usage(usage, 20, 6, Some(8));
}

#[test]
fn openhands_uses_complete_cassette_not_task_event() {
    let cassette = chat_cassette(2);
    let output = serde_json::json!({"kind":"ObservationEvent","content":"{\"usage\":{\"input_tokens\":999,\"output_tokens\":888}}"}).to_string();
    let usage = token_usage_from_sources("openrouter", "openhands", &output, "", Some(&cassette.0));
    assert_eq!(usage.source.as_deref(), Some("cassette"));
    assert_usage(usage, 20, 6, Some(8));
}

#[test]
fn incomplete_recordings_are_not_reported_as_complete_totals() {
    let complete = chat_cassette(1);
    let truncated = serde_json::json!({"request":{"url":"/v1/chat/completions"},"response":{"status":200,"body":"data: {\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":3}}\n\n"}});
    let cassette = Fixture::new(&format!(
        "{}{truncated}\n",
        std::fs::read_to_string(&complete.0).unwrap()
    ));
    let usage = token_usage_from_sources("openrouter", "continue", "", "", Some(&cassette.0));
    assert_eq!(usage.total_tokens, None);
}
