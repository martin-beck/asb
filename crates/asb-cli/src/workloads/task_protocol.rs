//! Private task protocol, above the generic JSON-lines transport. These types
//! preserve the existing wire and are not workload evaluation-kind registries.
use super::Evaluation;
use super::controller::decode_base64;
use super::session::{
    BaselineEvidence, DatasetIdentity, ProcessLifetime, ResourceDisposition, TaskIdentity,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::Path;

const DEFAULT_AGENT_TIMEOUT_SECONDS: f64 = 900.0;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum EventKind {
    Ready,
    Paused,
    PreparationFailed,
    Output,
    ExecutionEnded,
    Completed,
    Submission,
    Evaluated,
    Error,
    Closed,
    #[serde(rename = "initial-material")]
    InitialMaterial,
}

impl EventKind {
    pub fn of(value: &Value) -> Option<Self> {
        serde_json::from_value(value.get("event")?.clone()).ok()
    }
}

/// Only the operations emitted by the Rust task session. Python's additional
/// execute/capture/cancel operations remain available to its existing callers.
#[derive(Serialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub(super) enum Request<'a> {
    Agent {
        #[serde(flatten)]
        payload: AgentRequest<'a>,
    },
    Resume {
        exec_id: &'a Value,
    },
    Evaluate {
        timeout: u64,
    },
    Close,
    #[serde(rename = "initial-material")]
    InitialMaterial {
        binary: &'a Path,
    },
}

#[derive(Serialize)]
pub(super) struct AgentRequest<'a> {
    pub binary: &'a Path,
    pub project_root: &'a Path,
    pub preparation_seconds: Option<f64>,
    pub timeout: f64,
    pub tools_root: &'a Path,
    pub bootstrap: &'a crate::install::GuestBootstrap,
    pub measure_target: bool,
    pub project_configuration: String,
    pub environment: serde_json::Map<String, Value>,
    pub network: Option<String>,
    pub private_inputs: Vec<PrivateInput>,
    pub request: Value,
}

/// A runner-sealed, bounded host file staged only into the owned guest-private
/// directory.  The controller verifies both length and digest before upload.
#[derive(Serialize)]
pub(super) struct PrivateInput {
    pub source: std::path::PathBuf,
    pub target: String,
    pub bytes: u64,
    pub sha256: String,
}

impl Request<'_> {
    pub fn value(&self) -> Result<Value, String> {
        serde_json::to_value(self).map_err(|_| "cannot encode workload request".into())
    }
}

/// Retains the selected-instance validation and historical optional budget
/// fallback. Extra invocation fields remain forward compatible.
pub(super) struct ReadyTask {
    pub identity: TaskIdentity,
    pub prompt: String,
    pub guest_workspace: String,
    pub baseline: BaselineEvidence,
    pub agent_timeout: f64,
}

impl ReadyTask {
    pub fn decode(
        event: &Value,
        workload: &str,
        task: &str,
        dataset: &DatasetIdentity,
    ) -> Result<Self, String> {
        if EventKind::of(event) != Some(EventKind::Ready) {
            return Err("official task did not become ready".into());
        }
        let invocation = &event["invocation"];
        let identity: TaskIdentity = serde_json::from_value(invocation["identity"].clone())
            .map_err(|_| "official task returned invalid identity")?;
        if identity.workload != workload
            || identity.task_id != task
            || identity.dataset.as_ref() != Some(dataset)
        {
            return Err("prepared task does not match the selected dataset instance".into());
        }
        let prompt = invocation["prompt"]
            .as_str()
            .filter(|text| !text.is_empty() && text.len() <= 64 * 1024)
            .ok_or("official task returned invalid public instructions")?
            .to_owned();
        let guest_workspace = invocation["workspace"]
            .as_str()
            .filter(|path| path.starts_with('/') && !path.contains(".."))
            .ok_or("official task returned invalid environment workspace")?
            .to_owned();
        let baseline: BaselineEvidence = serde_json::from_value(invocation["baseline"].clone())
            .map_err(|_| "official baseline evidence is invalid")?;
        if !baseline.supports_evaluation(super::SavedEvaluationKind::Known(
            super::KnownEvaluationKind::UpstreamHarness,
        )) {
            return Err("official task definition does not establish baseline identity".into());
        }
        let agent_timeout = invocation
            .get("agent_timeout_seconds")
            .and_then(Value::as_f64)
            .unwrap_or(DEFAULT_AGENT_TIMEOUT_SECONDS);
        if !agent_timeout.is_finite() || !(0.001..=86400.0).contains(&agent_timeout) {
            return Err("official task returned an invalid agent time budget".into());
        }
        Ok(Self {
            identity,
            prompt,
            guest_workspace,
            baseline,
            agent_timeout,
        })
    }
}

/// A completion cannot reach evaluation until guest metadata confirms either
/// quiescence or the declared namespace-owned service handoff. Measurement
/// boundary/attachment checks remain in the measured execution phase.
pub(super) struct CompletedExecution {
    metadata: crate::agents::driver::GuestResult,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    code: i32,
    elapsed_ms: Option<u128>,
}

impl CompletedExecution {
    pub fn decode(event: &Value, lifetime: ProcessLifetime) -> Result<Self, String> {
        if EventKind::of(event) != Some(EventKind::Completed) {
            return Err("unexpected isolated execution event".into());
        }
        let result = &event["result"];
        let mut metadata: crate::agents::driver::GuestResult =
            serde_json::from_value(result["metadata"].clone())
                .map_err(|_| "guest returned invalid agent result metadata")?;
        if result.get("process_termination").is_some()
            || result.get("process_termination_diagnostic").is_some()
        {
            let forwarded = crate::process_termination::Observation::from_record(result);
            if forwarded.process_termination_diagnostic.is_some()
                || matches!((forwarded.process_termination, metadata.observation.process_termination), (Some(left), Some(right)) if left != right)
            {
                metadata.observation = crate::process_termination::Observation::malformed();
            }
        }
        let delegated = lifetime == ProcessLifetime::ThroughEvaluation
            && result["workload_processes_retained"].as_bool() == Some(true)
            && metadata.workload_owner.as_ref().is_some_and(|owner| {
                owner.controller_namespace > 1
                    && owner.guest_namespace > 1
                    && owner.controller_namespace != owner.guest_namespace
                    && owner.token.len() == 32
                    && owner.token.bytes().all(|byte| byte.is_ascii_hexdigit())
            });
        if !metadata.tree_quiesced && !delegated {
            return Err(
                "isolated agent process tree was not confirmed stopped before evaluation".into(),
            );
        }
        Ok(Self {
            metadata,
            stdout: decode_base64(&result["stdout"])?,
            stderr: decode_base64(&result["stderr"])?,
            code: result["exit_code"].as_i64().unwrap_or(1).clamp(0, 255) as i32,
            elapsed_ms: result["elapsed_seconds"]
                .as_f64()
                .filter(|seconds| seconds.is_finite() && (0.0..=86410.0).contains(seconds))
                .map(|seconds| (seconds * 1000.0).round() as u128),
        })
    }

    pub fn metadata(&self) -> &crate::agents::driver::GuestResult {
        &self.metadata
    }

    pub fn into_execution(self) -> Result<super::session::ExecutionResult, String> {
        #[cfg(unix)]
        let status = {
            use std::os::unix::process::ExitStatusExt;
            std::process::ExitStatus::from_raw(self.code << 8)
        };
        #[cfg(not(unix))]
        return Err("isolated execution requires Unix".into());
        Ok(super::session::ExecutionResult {
            collector_errors: Vec::new(),
            output_error: None,
            output: std::process::Output {
                status,
                stdout: self.stdout,
                stderr: self.stderr,
            },
            metadata: self.metadata,
            elapsed_ms: self.elapsed_ms,
        })
    }
}

/// One matching latest official binary grade permits one normal close. This
/// transport does not reinterpret native rewards or the adapter extension API.
/// No Clone/default/public fields: callers cannot mint or reuse close permission.
pub(super) struct CompletedGrade {
    resolved: bool,
}

impl CompletedGrade {
    pub fn decode(event: &Value, expected: &TaskIdentity) -> Result<Self, String> {
        if EventKind::of(event) != Some(EventKind::Evaluated) {
            return Err("unexpected workload controller response".into());
        }
        let result = &event["result"];
        if result["workload"] != expected.workload
            || result["task_id"] != expected.task_id
            || result["dataset"]
                != serde_json::to_value(&expected.dataset)
                    .map_err(|_| "cannot compare evaluation dataset identity")?
        {
            return Err("official evaluation does not match this dataset instance".into());
        }
        match (result["outcome"].as_str(), result["resolved"].as_bool()) {
            (Some("resolved"), Some(true)) => Ok(Self { resolved: true }),
            (Some("unresolved" | "empty_submission"), Some(false)) => Ok(Self { resolved: false }),
            _ => Err("official evaluator did not return a completed task-specific grade; inspect task artifacts".into()),
        }
    }

    pub fn evaluation(&self) -> Evaluation {
        if self.resolved {
            Evaluation::Passed
        } else {
            Evaluation::Failed("official evaluator did not resolve the selected instance".into())
        }
    }
}

pub(super) struct NormalCloseAck {
    disposition: ResourceDisposition,
}
impl NormalCloseAck {
    pub fn decode(event: &Value) -> Result<Self, String> {
        // A positive preservation acknowledgement is settled but is not a
        // successful normal teardown. Controller::finish retains that diagnostic.
        Ok(Self {
            disposition: CancellationAck::decode(event)?.disposition(),
        })
    }
    pub fn disposition(&self) -> ResourceDisposition {
        self.disposition
    }
}

pub(super) struct CancellationAck {
    disposition: ResourceDisposition,
}
impl CancellationAck {
    pub fn decode(event: &Value) -> Result<Self, String> {
        if EventKind::of(event) != Some(EventKind::Closed) {
            return Err("cancelled workload cleanup did not confirm removal or stopped preservation; inspect private task cleanup artifacts".into());
        }
        let disposition = match event["cleanup"].as_str() {
            Some("removed") => ResourceDisposition::Removed,
            Some("not_created") => ResourceDisposition::NotCreated,
            Some("stopped-preserved") => ResourceDisposition::StoppedPreserved,
            _ => return Err("cancelled workload cleanup did not confirm removal or stopped preservation; inspect private task cleanup artifacts".into()),
        };
        Ok(Self { disposition })
    }
    pub fn disposition(&self) -> ResourceDisposition {
        self.disposition
    }
}

#[cfg(test)]
#[path = "../../tests/unit/task_protocol.rs"]
mod tests;
