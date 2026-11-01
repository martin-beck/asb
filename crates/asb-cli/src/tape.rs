//! Record/replay entry point, backed by the configured proxy chain.

use crate::{Error, proxy};
use serde::{Deserialize, Serialize};
use std::path::Path;

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
    pub agent: Option<&'a str>,
    pub recorder: Option<&'a str>,
    pub invocation_dir: &'a Path,
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
                agent,
                recorder,
                invocation_dir: Path::new("."),
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
        let settings = proxy::RecordingRequest {
            recorder: context.recorder,
            mode,
            cassette,
            respect_record_timings,
            workspace: context.workspace,
            invocation_dir: context.invocation_dir,
            wire_extension: context.agent.and_then(crate::agents::tape_wire_extension),
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
