//! Record/replay entry point, backed by the configured proxy chain.

use crate::{Error, proxy};
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::path::{Path, PathBuf};

/// Request normalization, ordered matching and full-consumption replay semantics.
/// Bump for incompatible tape matching changes, not scheduling/measurement changes.
pub(crate) const REPLAY_MATCHING_CONTRACT: &str = "asb-tape-request-matching-v1";

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum Mode {
    Record,
    Replay,
}

/// A running record/replay chain.
pub struct Session(proxy::Chain);

pub(crate) struct AgentContext<'a> {
    pub workspace: &'a Path,
    pub ignore_provider_mismatch: bool,
    pub ignore_provider_unconsumed: bool,
    pub agent: Option<&'a str>,
    /// The configured workload selects only its adapter's explicitly declared
    /// cassette normalizers. It is not inferred from request data.
    pub workload: Option<&'a str>,
    pub task: Option<&'a str>,
    pub custom_normalization: Option<proxy::CustomNormalization>,
    pub recorder: Option<&'a str>,
    pub invocation_dir: &'a Path,
    /// Collector-owned observation endpoint for ASB's embedded proxy only.
    pub perfetto_observer: Option<File>,
    pub perfetto_trace_id: Option<String>,
    pub perfetto_report: Option<PathBuf>,
}

impl Session {
    #[cfg(test)]
    pub fn start(
        mode: Mode,
        cassette: &Path,
        upstream: Option<&str>,
        respect_record_timings: bool,
        workspace: &Path,
    ) -> Result<Self, Error> {
        Self::start_for_agent(
            mode,
            cassette,
            upstream,
            respect_record_timings,
            workspace,
            None,
            None,
        )
    }

    /// Start a tape session with an optional adapter-owned opaque wire
    /// extension. The tape itself never interprets that extension's protocol.
    #[cfg(test)]
    pub fn start_for_agent(
        mode: Mode,
        cassette: &Path,
        upstream: Option<&str>,
        respect_record_timings: bool,
        workspace: &Path,
        agent: Option<&str>,
        recorder: Option<&str>,
    ) -> Result<Self, Error> {
        Self::start_for_agent_at(
            mode,
            cassette,
            upstream,
            respect_record_timings,
            AgentContext {
                workspace,
                ignore_provider_mismatch: false,
                ignore_provider_unconsumed: false,
                agent,
                workload: None,
                task: None,
                custom_normalization: None,
                recorder,
                invocation_dir: Path::new("."),
                perfetto_observer: None,
                perfetto_trace_id: None,
                perfetto_report: None,
            },
        )
    }

    pub fn start_for_agent_at(
        mode: Mode,
        cassette: &Path,
        upstream: Option<&str>,
        respect_record_timings: bool,
        context: AgentContext<'_>,
    ) -> Result<Self, Error> {
        if mode == Mode::Replay {
            require_replay_cassette(cassette)?;
        }
        let normalization_scripts = match context.agent.zip(context.workload) {
            Some((agent, workload)) => {
                crate::agents::tape_normalization_scripts(agent, workload, context.task)
            }
            None => {
                // `Session::start` is a test-only legacy helper whose fixtures
                // predate adapter ownership. Keep those unit fixtures useful
                // by exercising every registered module; production paths
                // always provide both identities above and never take this
                // branch.
                #[cfg(test)]
                {
                    crate::agents::all()
                        .iter()
                        .flat_map(|adapter| adapter.tape_normalization_scripts("fixture", None))
                        .collect()
                }
                #[cfg(not(test))]
                {
                    Vec::new()
                }
            }
        };
        let settings = proxy::RecordingRequest {
            recorder: context.recorder,
            mode,
            cassette,
            respect_record_timings,
            ignore_provider_mismatch: context.ignore_provider_mismatch,
            ignore_provider_unconsumed: context.ignore_provider_unconsumed,
            workspace: context.workspace,
            invocation_dir: context.invocation_dir,
            discovery_workload: context.workload,
            discovery_task: context.task,
            wire_extension: context.agent.and_then(crate::agents::tape_wire_extension),
            normalization_scripts,
            custom_normalization: context.custom_normalization,
            perfetto_observer: context.perfetto_observer,
            perfetto_trace_id: context.perfetto_trace_id,
            perfetto_report: context.perfetto_report,
        };
        proxy::start_recording_chain(&settings, upstream).map(Self)
    }

    pub fn url(&self) -> &str {
        self.0.url()
    }

    #[cfg(test)]
    pub fn stop(self) -> Result<(), Error> {
        self.0.stop()
    }

    pub(crate) fn finish(self) -> proxy::Completion {
        self.0.finish()
    }
}

/// Validate a replay input before allocating run resources or starting a proxy.
pub(crate) fn require_replay_cassette(cassette: &Path) -> Result<(), Error> {
    let mut present = crate::cassette::Source::is_present(cassette)
        .map_err(|error| Error::Input(error.to_string()))?;
    for replica in 1..=crate::benchmark::MAX_CONCURRENCY {
        present |= crate::cassette::Source::is_present(&crate::benchmark_paths::replica_path(
            cassette, replica,
        ))
        .map_err(|error| Error::Input(error.to_string()))?;
    }
    if present {
        Ok(())
    } else {
        Err(Error::Input(format!(
            "replay cassette {} does not exist; run `asb run CONFIG --record --cassette {}` first",
            cassette.display(),
            cassette.display()
        )))
    }
}
