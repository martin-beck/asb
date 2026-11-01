//! Recorder-neutral provider tool-call observations.
use serde::Serialize;

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct ToolCall {
    pub ordinal: usize,
    /// Recorder-owned source, never an agent presentation stream.
    pub source: &'static str,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub arguments_present: bool,
    /// Exact proxy-observed wall-clock time at which the provider response was
    /// completely received, in Unix milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_observed_unix_ms: Option<u128>,
    /// The exact, observable elapsed time from this response to the next
    /// request carrying this call's result.  `None` means the protocol did not
    /// expose a matching result turn; it is not guessed from process output.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub execution_duration_ms: Option<u128>,
    /// Stable, multi-label operation taxonomy. Arguments are read only long
    /// enough to classify a call and are never persisted.
    pub classes: Vec<String>,
}
