//! Side-effect-free persisted execution layout shared by run, sweep and reports.
//! Deserialization is not admission: consumers must validate before activation.
use crate::{benchmark::BenchmarkConfig, planning};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

mod layout;
pub(super) use layout::{MAX_EXECUTIONS, planned_slots};
pub(crate) mod generation;
pub(crate) mod reading;

/// Exact retained campaign plan beneath an owning sweep's results directory.
/// This references intended slots, never completion or continuation authority.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Reference {
    pub root: PathBuf,
    pub bytes: u64,
    pub sha256: String,
}

/// The same execution identity is emitted by the runner and checked by readers.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExecutionIdentity {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) replay_track: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) replay_seed: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) replay_algorithm: Option<String>,
    pub(super) admission: planning::BatchAdmissionPolicy,
    pub(super) campaign: String,
    pub(super) batch: u64,
    pub(super) replica: usize,
    pub(super) slot: u64,
    pub(super) total: u64,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OutcomeReference {
    pub schema_version: u32,
    pub execution: ExecutionIdentity,
    pub path: PathBuf,
    pub bytes: u64,
    pub sha256: String,
    pub task: serde_json::Value,
}

/// A pre-allocation start declaration, never proof that an agent was launched.
/// Historical records omit the cohort; that absence is not resume authority.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StartIntent {
    pub schema_version: u32,
    pub scope: String,
    pub execution: ExecutionIdentity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_cohort: Option<crate::measurements::ExecutionCohort>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<generation::StartReference>,
}

#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Slot {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub replay_track: Option<u64>,
    pub id: u64,
    pub batch: u64,
    pub replica: usize,
    pub workspace: PathBuf,
    pub log: PathBuf,
    pub measurements: Option<PathBuf>,
    pub replay_input: Option<usize>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReplayInput {
    /// Historical tracks omit this and use the legacy llmtape format.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recorder: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub complete: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub track: Option<u64>,
    pub source: PathBuf,
    pub private: PathBuf,
    /// A verified, private staging location for the causal local-tool payload.
    /// Historical or provider-only recordings legitimately omit it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_tools: Option<PathBuf>,
    /// Adapter-owned local-tool protocol identity staged beside its selected
    /// replay track. It lets a client reject a changed tool definition before
    /// emitting provider requests that can never match the recording.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_protocol: Option<PathBuf>,
    /// A verified deterministic-execution manifest staged beside its replay
    /// track. Adapters consume it only when they selected that extension.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deterministic_manifest: Option<PathBuf>,
    /// A verified native-trace index staged beside its exact replay track.
    /// Older recordings omit it until a native capture backend is selected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_trace: Option<PathBuf>,
    /// Owner-only selected v2 native-tool sidecar. This is deliberately not
    /// serialized into campaign plans or public result data.
    #[serde(skip)]
    pub native_tool: Option<PathBuf>,
    /// Payload-free authority derived during private staging.  Campaign plans
    /// intentionally retain neither this descriptor nor its sidecar path.
    #[cfg(not(test))]
    #[serde(skip)]
    pub native_tool_launch: Option<crate::cassette::NativeToolLaunch>,
    /// Sealed generic MCP relay bundles, addressed only by their declared
    /// stable server id. A replay adapter must reject a requested server whose
    /// bundle is absent rather than launch its original program.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mcp_relays: Vec<McpRelayInput>,
    pub bytes: u64,
    pub sha256: String,
}

impl ReplayInput {
    #[cfg(not(test))]
    pub(crate) fn native_tool_launch(&self) -> Option<&crate::cassette::NativeToolLaunch> {
        self.native_tool_launch.as_ref()
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub(crate) fn native_tool_launch(&self) -> Option<&crate::cassette::NativeToolLaunch> {
        None
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct McpRelayInput {
    pub id: String,
    pub bundle: PathBuf,
}

/// Whether measurements move with their owned campaign or remain at an explicit
/// external override. Historical plans omit this distinction; readers do not guess.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum MeasurementStorage {
    Campaign,
    Override,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Plan {
    /// Expected source identity, not adapter-observed prepared material.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replay_compatibility: Option<crate::cassette::Compatibility>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recording_selection: Option<crate::workloads::TaskSelection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recording_reservation: Option<crate::cassette::RecordingReservation>,
    /// Nominal source preserves sparse legacy routing, not just its first file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replay_source: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub measurement_storage: Option<MeasurementStorage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub replay_selection: Option<crate::cassette::SelectionPlan>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recording: Option<PathBuf>,
    pub admission: planning::BatchAdmissionPolicy,
    pub schema_version: u32,
    pub scope: String,
    pub campaign: String,
    pub source: PathBuf,
    pub configuration: BenchmarkConfig,
    pub configuration_sha256: String,
    pub repeat: u64,
    pub concurrency: usize,
    pub mode: planning::ExecutionMode,
    pub respect_record_timings: bool,
    pub measurement_root: Option<PathBuf>,
    pub replay_inputs: Vec<ReplayInput>,
    pub slots: Vec<Slot>,
}

impl Plan {
    /// Recompute capacity from requested names and paths, never trust serialized
    /// capacity as execution evidence. Historical plans lack both fields.
    pub(crate) fn recording_capacity(
        &self,
    ) -> Result<Option<crate::cassette::RecordingReservation>, crate::Error> {
        let Some(selection) = &self.recording_selection else {
            return Ok(None);
        };
        let config = &self.configuration;
        if self.mode != planning::ExecutionMode::Record
            || !selection.valid()
            || selection.workload != config.workload
            || selection.task_id != config.trial.as_deref().unwrap_or(&config.workload)
        {
            return Err(crate::Error::Input(
                "recording capacity selection differs from campaign".into(),
            ));
        }
        crate::cassette::RecordingReservation::for_plan(
            crate::cassette::CompatibilityShape {
                agent: &config.agent,
                provider: &config.provider,
                model: &config.model,
                workload: &config.workload,
                task: &selection.task_id,
            },
            &self.campaign,
            self.recording.as_deref().ok_or_else(|| {
                crate::Error::Input("recording capacity has no destination".into())
            })?,
            self.repeat,
            self.concurrency as u64,
            crate::cassette::Limits::default(),
        )
        .map(Some)
        .map_err(|error| crate::Error::Input(error.to_string()))
    }

    pub(crate) fn execution(&self, slot: &Slot) -> ExecutionIdentity {
        ExecutionIdentity {
            replay_track: slot.replay_track,
            replay_seed: self.replay_selection.as_ref().map(|value| value.seed),
            replay_algorithm: self
                .replay_selection
                .as_ref()
                .map(|value| value.algorithm.clone()),
            admission: self.admission,
            campaign: self.campaign.clone(),
            batch: slot.batch,
            replica: slot.replica,
            slot: slot.id,
            total: self.slots.len() as u64,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn owned_campaign_retains_existing_wire_shapes_without_invented_replay_facts() {
        let configuration: BenchmarkConfig = toml::from_str(
            "agent='pi'\nprovider='openrouter'\nmodel='fixture'\nworkload='bug-fix'",
        )
        .unwrap();
        let wire = json!({
            "admission": planning::BatchAdmissionPolicy::current(),
            "schema_version": 1,
            "scope": "intended-executions-not-attempt-evidence",
            "campaign": "fixture", "source": "/project/benchmark.toml",
            "configuration": configuration,
            "configuration_sha256": "a".repeat(64),
            "repeat": 1, "concurrency": 1, "mode": "replay",
            "respect_record_timings": false, "measurement_root": null,
            "replay_inputs": [{"source":"/project/legacy.jsonl", "private":"inputs/1.jsonl", "bytes":42, "sha256":"b".repeat(64)}],
            "slots": [{"id":1,"batch":1,"replica":1,"workspace":"batch-1/workspace", "log":"batch-1/run.jsonl", "measurements":null,"replay_input":0}]
        });
        let plan: Plan = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(serde_json::to_value(plan).unwrap(), wire);
        let mut bundle = wire;
        bundle["replay_inputs"][0]["complete"] = json!(false);
        bundle["replay_inputs"][0]["track"] = json!(7);
        bundle["slots"][0]["replay_track"] = json!(7);
        let plan: Plan = serde_json::from_value(bundle.clone()).unwrap();
        assert_eq!(serde_json::to_value(plan).unwrap(), bundle);
        for location in ["plan", "slot", "input"] {
            let mut changed = bundle.clone();
            match location {
                "slot" => changed["slots"][0]["unexpected"] = json!(true),
                "input" => changed["replay_inputs"][0]["unexpected"] = json!(true),
                _ => changed["unexpected"] = json!(true),
            }
            assert!(
                serde_json::from_value::<Plan>(changed).is_err(),
                "{location}"
            );
        }
    }
}
