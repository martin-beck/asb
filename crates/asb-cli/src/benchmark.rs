//! Shared benchmark configuration.

use serde::{Deserialize, Serialize};

/// Small, extensible benchmark configuration written by `asb setup`.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct BenchmarkConfig {
    pub agent: String,
    pub workload: String,
    pub provider: String,
    pub model: String,
    #[serde(default = "default_workspace")]
    pub workspace: String,
    #[serde(default = "default_log_file")]
    pub log_file: String,
    #[serde(default)]
    pub measurements: Option<MeasurementConfig>,
}

/// Measurement modules selected by generated setup files.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct MeasurementConfig {
    pub enabled: bool,
    pub directory: String,
    pub collectors: Vec<String>,
}

fn default_workspace() -> String {
    "./asb-workspace".into()
}

fn default_log_file() -> String {
    "benchmark.log".into()
}
