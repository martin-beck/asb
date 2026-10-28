//! Record/replay entry point, backed by the configured proxy chain.

use crate::{Error, proxy};
use serde::{Deserialize, Serialize};
use std::any::Any;
use std::collections::HashMap;
use std::path::Path;

/// Request normalization, ordered matching and full-consumption replay semantics.
/// Bump for incompatible tape matching changes, not scheduling/measurement changes.
pub(crate) const REPLAY_MATCHING_CONTRACT: &str = "asb-tape-request-matching-v1";

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum Mode {
    Record,
    Replay,
}

pub(crate) fn input_files() -> Result<Vec<std::path::PathBuf>, Error> {
    proxy::input_files(proxy::adapters::llmtape::NAME)
}
pub(crate) fn runtime_inputs() -> Result<Vec<crate::runtime_inputs::Dependency>, Error> {
    proxy::runtime_inputs(proxy::adapters::llmtape::NAME)
}

/// A running record/replay chain.
pub struct Session(proxy::Chain);

impl Session {
    pub fn start(
        mode: Mode,
        cassette: &Path,
        upstream: Option<&str>,
        respect_record_timings: bool,
        workspace: &Path,
    ) -> Result<Self, Error> {
        if mode == Mode::Replay {
            require_replay_cassette(cassette)?;
        }
        let settings = proxy::adapters::llmtape::Settings {
            mode,
            cassette: cassette.to_path_buf(),
            respect_record_timings,
            workspace: workspace.to_path_buf(),
        };
        let mut configurations: HashMap<String, Box<dyn Any>> = HashMap::new();
        configurations.insert(proxy::adapters::llmtape::NAME.into(), Box::new(settings));
        let names = proxy::configured_chain(proxy::adapters::llmtape::NAME)?;
        proxy::Chain::start(&names, upstream, &configurations).map(Self)
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
