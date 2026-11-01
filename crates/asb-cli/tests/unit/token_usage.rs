use super::*;
use crate::proxy::usage::from_cassette as usage_from_cassette;
use serde_json::json;

#[test]
fn human_token_summaries_normalize_units_and_reject_invalid_counts() {
    let usage =
        usage_from_key_values("1.5k sent; 2.25k received; cached tokens: 10; total tokens: 4k");
    assert_eq!(usage.input_tokens, Some(1500));
    assert_eq!(usage.output_tokens, Some(2250));
    assert_eq!(usage.cached_tokens, Some(10));
    assert_eq!(usage.total_tokens, Some(4000));
    for (raw, value) in [
        ("1.2m", Some(1_200_000)),
        ("'2k'", Some(2000)),
        ("3.5", Some(4)),
        ("-3", None),
        ("NaN", None),
        ("inf", None),
        ("1e100", None),
        ("", None),
    ] {
        assert_eq!(parse_token_count(raw), value, "{raw}");
    }
    let usage = usage_from_text("output tokens=99\n");
    assert_eq!(usage.output_tokens, Some(99));
}

#[test]
fn embedded_events_handle_escaped_quotes_braces_and_incomplete_objects() {
    let first =
        json!({"message":"brace } and quote \"","usage":{"input_tokens":4,"output_tokens":2}});
    let second = json!({"usage":{"input_tokens":7,"output_tokens":3}});
    let text = format!("prefix {first} suffix {second} {{\"incomplete\":");
    let events = embedded_json_values(&text);
    assert_eq!(events.len(), 2);
    assert_eq!(events[0]["usage"]["input_tokens"], 4);
    assert_eq!(events[1]["usage"]["input_tokens"], 7);
    let usage = usage_from_text(&format!("prefix {first} suffix {second}"));
    assert_eq!(usage.input_tokens, Some(11));
    assert_eq!(usage.output_tokens, Some(5));
}

#[test]
fn partial_or_irrelevant_cassettes_do_not_invent_complete_usage() {
    for record in ["not-json".into(), "{}".into(), json!({"response":{"status":200}}).to_string(), json!({"request":{"url":"http://fixture.invalid/models"},"response":{"status":200,"body":"{}"}}).to_string(), json!({"response":{"status":401,"body":"{}"}}).to_string()] {
        assert!(!usage_from_cassette(&record, false).any());
    }
    let partial = format!("data: {}\n\n", json!({"usage":{"input_tokens":2}}));
    let zero = json!({"usage":{"input_tokens":0,"output_tokens":0}}).to_string();
    for body in [partial, zero] {
        assert!(!usage_from_cassette(&json!({"response":{"body":body}}).to_string(), false).any());
    }
}

#[test]
fn usage_artifacts_are_bounded_and_missing_data_is_unavailable() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/token-usage-fixtures")
        .join(crate::workloads::TaskIdentity::local("fixture", None).attempt_id);
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join("usage");
    assert!(!from_path(&path).any());
    std::fs::write(&path, b"input tokens=33; output tokens=7").unwrap();
    let usage = from_path(&path);
    assert_eq!(usage.total_tokens, Some(40));
    assert!(
        read_bounded_text(&path, 3)
            .unwrap_err()
            .to_string()
            .contains("size limit")
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn registered_agents_own_their_output_usage_schema() {
    for agent in crate::agents::all() {
        assert!(
            agent.output_usage("", "", false).is_some(),
            "{} lacks an output usage hook",
            agent.id()
        );
    }
}

#[test]
fn unregistered_agent_output_is_not_interpreted_as_usage() {
    let usage = from_sources(
        "unknown",
        "unregistered-agent",
        r#"{"usage":{"input_tokens":123,"output_tokens":456}}"#,
        "",
        None,
    );
    assert!(!usage.any());
}

#[test]
fn adapter_without_usage_hook_does_not_parse_task_output() {
    struct Unowned;
    impl crate::agents::AgentAdapter for Unowned {
        fn id(&self) -> &'static str {
            "unowned"
        }
        fn executable(&self) -> &'static str {
            unreachable!()
        }
        fn recipe(&self) -> crate::install::Recipe {
            unreachable!()
        }
        fn supported_endpoints(&self) -> &'static [crate::endpoint::Support] {
            unreachable!()
        }
        fn command(&self, _: &crate::agents::CommandContext<'_>) -> crate::agents::CommandSpec {
            unreachable!()
        }
        fn prepare(
            &self,
            _: &mut crate::agents::PreparationContext<'_>,
        ) -> Result<(), crate::Error> {
            unreachable!()
        }
    }
    let output = r#"{"usage":{"input_tokens":123,"output_tokens":456}}"#;
    assert!(!adapter_output_usage(Some(&Unowned), output, output, false).any());
}

#[test]
fn run_evidence_takes_precedence_over_inherited_environment() {
    const CHILD: &str = "ASB_USAGE_PRECEDENCE_TEST_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "usage::normalization_tests::run_evidence_takes_precedence_over_inherited_environment", "--nocapture"])
            .env(CHILD, "1")
            .env("ASB_INPUT_TOKENS", "999")
            .env("ASB_OUTPUT_TOKENS", "888")
            .env("ASB_CACHED_TOKENS", "777")
            .output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let path = std::env::temp_dir()
        .join(crate::workloads::TaskIdentity::local("usage-priority", None).attempt_id);
    let body = json!({"usage":{"input_tokens":4,"output_tokens":2}}).to_string();
    std::fs::write(
        &path,
        json!({"request":{"url":"/v1/responses"},"response":{"status":200,"body":body}})
            .to_string(),
    )
    .unwrap();
    let usage = from_sources("openrouter", "pi", "", "", Some(&path));
    std::fs::remove_file(path).unwrap();
    assert_eq!(
        (
            usage.input_tokens,
            usage.output_tokens,
            usage.cached_tokens,
            usage.total_tokens
        ),
        (Some(4), Some(2), None, Some(6))
    );
    assert_eq!(usage.source.as_deref(), Some("cassette"));
    let output = r#"[{"type":"result","usage":{"input_tokens":12,"output_tokens":5}}]"#;
    let observed = from_sources("openrouter", "qwen-code", output, "", None);
    assert_eq!(
        (
            observed.input_tokens,
            observed.output_tokens,
            observed.cached_tokens,
            observed.total_tokens
        ),
        (Some(12), Some(5), None, Some(17))
    );
    assert_eq!(observed.source.as_deref(), Some("agent-output"));
    let fallback = from_sources("openrouter", "pi", "", "", None);
    assert_eq!(fallback.input_tokens, Some(999));
    let artifact = TokenUsage {
        input_tokens: Some(10),
        output_tokens: Some(3),
        ..Default::default()
    };
    let usage = with_artifact_usage(fallback, Some(artifact));
    assert_eq!(
        (
            usage.input_tokens,
            usage.output_tokens,
            usage.cached_tokens,
            usage.total_tokens
        ),
        (Some(10), Some(3), None, Some(13))
    );
    assert_eq!(usage.source.as_deref(), Some("agent-artifact"));
    let partial = TokenUsage {
        input_tokens: Some(6),
        ..Default::default()
    };
    let usage = with_artifact_usage(
        from_sources("openrouter", "pi", "", "", None),
        Some(partial),
    );
    assert_eq!(
        (
            usage.input_tokens,
            usage.output_tokens,
            usage.cached_tokens,
            usage.total_tokens
        ),
        (Some(6), None, None, None)
    );
}
