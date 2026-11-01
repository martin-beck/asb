//! Registry and individual agent adapters.

use super::*;

mod claude_code;
mod cline;
mod codex;
mod continue_agent;
pub(super) mod goose;
mod hermes;
mod mini_swe;
mod opencode;
mod openhands;
pub(crate) mod openhands_native;
mod openjiuwen;
mod pi;
mod qwen_code;
pub(super) mod replay_artifacts;
mod workbuddy;

/// Classify only an adapter-extracted terminal answer, never a tool transcript.
fn terminal_refusal(response: Option<&str>) -> Option<String> {
    let text = response?.trim_start().to_lowercase();
    if [
        "i can't run tests",
        "i can’t run tests",
        "i cannot run tests",
    ]
    .iter()
    .any(|prefix| text.starts_with(prefix))
    {
        return Some(
            "agent reported it could not run tests; inspect the benchmark log and test environment"
                .into(),
        );
    }
    if [
        "i can't inspect",
        "i cannot inspect",
        "i don't have access to the terminal",
        "i do not have access to the terminal",
        "i can't modify files",
        "i cannot modify files",
        "the workspace is read-only",
        "the workspace appears to be read-only",
    ]
    .iter()
    .any(|prefix| text.starts_with(prefix))
    {
        return Some("agent reported it could not access the terminal or workspace, or could not modify files; inspect the benchmark log and permissions".into());
    }
    None
}

pub(super) static REGISTRY: &[&dyn AgentAdapter] = &[
    &codex::ADAPTER,
    &pi::ADAPTER,
    &opencode::ADAPTER,
    &workbuddy::ADAPTER,
    &qwen_code::ADAPTER,
    &mini_swe::ADAPTER,
    &openhands::ADAPTER,
    &openjiuwen::ADAPTER,
    &claude_code::ADAPTER,
    &goose::ADAPTER,
    &hermes::ADAPTER,
    &cline::ADAPTER,
    &continue_agent::ADAPTER,
];

#[cfg(test)]
mod terminal_refusal_tests {
    use super::*;
    #[test]
    fn terminal_refusals_are_narrow_and_registered_by_owning_adapters() {
        for adapter in [&pi::ADAPTER, &claude_code::ADAPTER] {
            assert!(
                adapter
                    .final_response_failure(Some(
                        "I can't run tests because the tool is unavailable"
                    ))
                    .is_some(),
                "{}",
                adapter.id()
            );
            assert!(
                adapter
                    .final_response_failure(Some("I cannot inspect the workspace"))
                    .is_some(),
                "{}",
                adapter.id()
            );
            assert!(
                adapter
                    .final_response_failure(Some(
                        "The test checks the phrase cannot inspect in an error message."
                    ))
                    .is_none(),
                "{}",
                adapter.id()
            );
        }
    }
}
