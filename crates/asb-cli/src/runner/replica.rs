//! Stable identity and completion notification for one execution replica.
use crate::Error;
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Identity {
    pub index: usize,
    pub total: usize,
}

impl Default for Identity {
    fn default() -> Self {
        Self { index: 1, total: 1 }
    }
}

impl Identity {
    pub(crate) fn valid(self) -> bool {
        (1..=crate::benchmark::MAX_CONCURRENCY).contains(&self.total)
            && (1..=self.total).contains(&self.index)
    }
}

/// Optional persistence observers never select an agent, provider or workload.
/// Failed metadata publication cannot change a freshly computed task grade.
pub(crate) trait CompletionObserver: Sync {
    fn allocated(
        &self,
        _replica: Identity,
        _reference: &crate::workloads::recovery::AttemptReference,
    ) -> Result<(), Error> {
        Ok(())
    }
    /// Persist execution intent before measurement/provider/workload startup.
    /// Unlike optional completion metadata, failure must stop this attempt
    /// before any process or environment is started under the observer.
    fn starting(
        &self,
        _replica: Identity,
        _cohort: Option<&crate::measurements::ExecutionCohort>,
    ) -> Result<(), Error> {
        Ok(())
    }
    fn completed(&self, replica: Identity, record: &Path) -> Result<(), Error>;
    /// Optional receipt destination, for truthful publication diagnostics.
    /// Unknown observers must not mislabel the source outcome as a receipt.
    fn evidence_destination(&self, _replica: Identity) -> Option<std::path::PathBuf> {
        None
    }
}

pub(crate) fn configuration(
    config: &crate::benchmark::BenchmarkConfig,
    replica: Identity,
) -> Result<crate::benchmark::BenchmarkConfig, Error> {
    if !replica.valid() || replica.total != config.concurrency {
        return Err(Error::Input(
            "replica identity does not match the selected configuration".into(),
        ));
    }
    let mut selected = config.clone();
    selected.concurrency = 1;
    if replica.total > 1 {
        selected.workspace = super::parallel_path(&config.workspace, replica.index);
        selected.log_file = super::parallel_path(&config.log_file, replica.index);
        if let Some(measurements) = selected.measurements.as_mut() {
            measurements.directory = super::parallel_path(&measurements.directory, replica.index);
        }
    }
    Ok(selected)
}

pub(crate) fn runtime_path(workspace: &Path) -> std::path::PathBuf {
    let mut path = workspace.as_os_str().to_os_string();
    path.push(".asb-agent-runtime");
    path.into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replica_slots_are_bounded_one_based_and_preserve_selected_paths() {
        let config: crate::benchmark::BenchmarkConfig = toml::from_str(
            "agent='fixture'\nprovider='fixture'\nmodel='fixture'\nworkload='bug-fix'\nconcurrency=512\nworkspace='a/workspace'\nlog_file='a/run.log'\n[measurements]\nenabled=true\ndirectory='a/measures'\ncollectors=[]\n").unwrap();
        for (index, total, valid) in [
            (0, 512, false),
            (1, 0, false),
            (513, 512, false),
            (1, 513, false),
            (1, 512, true),
            (512, 512, true),
        ] {
            let identity = Identity { index, total };
            assert_eq!(identity.valid(), valid);
            let replica = configuration(&config, identity);
            assert_eq!(replica.is_ok(), valid);
            if let Ok(replica) = replica {
                assert_eq!(replica.concurrency, 1);
                assert_eq!(
                    replica.workspace,
                    format!("a/workspace-concurrency-{index}")
                );
                assert_eq!(replica.log_file, format!("a/run-concurrency-{index}.log"));
                assert_eq!(
                    replica.measurements.unwrap().directory,
                    format!("a/measures-concurrency-{index}")
                );
            }
        }
        let mut single = config.clone();
        single.concurrency = 1;
        assert_eq!(configuration(&single, Identity::default()).unwrap(), single);
        assert!(configuration(&config, Identity::default()).is_err());
        assert_eq!(
            runtime_path(Path::new("a/workspace")),
            Path::new("a/workspace.asb-agent-runtime")
        );
    }
}
