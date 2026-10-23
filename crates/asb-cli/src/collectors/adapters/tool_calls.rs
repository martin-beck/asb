//! Structured agent tool-call observations are published by measurement finalization.

use super::super::CollectorAdapter;

pub(super) struct ToolCallsCollector;
pub(super) static ADAPTER: ToolCallsCollector = ToolCallsCollector;

impl CollectorAdapter for ToolCallsCollector {
    fn id(&self) -> &'static str {
        "tool-calls"
    }
}
