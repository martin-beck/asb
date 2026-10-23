//! Registry and individual agent adapters.

use super::*;

mod claude_code;
mod cline;
mod codex;
mod continue_agent;
mod goose;
mod hermes;
mod mini_swe;
mod opencode;
mod openhands;
mod openjiuwen;
mod pi;
mod qwen_code;
pub(crate) mod tool_calls;
mod workbuddy;

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
