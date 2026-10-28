//! Shared contract for source-bundled external workload catalogs.
//!
//! An adapter supplies a bounded, local parser for its pinned source tree.
//! The generic installer downloads that source once, invokes the parser, and
//! publishes the resulting catalog beside the source.  Later consumers load
//! the persisted catalog and compare it with a fresh local enumeration, so a
//! stale or substituted catalog is never presented as an installed workload.

use super::{SetupExtension, SourceBundle, WorkloadAdapter};
use crate::workloads::Trial;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

const SCHEMA_VERSION: u32 = 1;
const MAX_CATALOG_BYTES: u64 = 16 * 1024 * 1024;
const MAX_TASKS: usize = 100_000;

pub(super) type Discover = fn(&Path) -> Result<Vec<Trial>, String>;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Catalog {
    schema_version: u32,
    workload: String,
    marker: String,
    tasks: Vec<CatalogTask>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CatalogTask {
    id: String,
    description: String,
}

/// An external suite whose official trial population is entirely represented
/// in its pinned source archive.
pub(crate) struct SourceCatalogAdapter {
    id: &'static str,
    source: &'static str,
    bundle: SourceBundle,
    discover: Discover,
}

impl SourceCatalogAdapter {
    pub(crate) const fn new(
        id: &'static str,
        source: &'static str,
        bundle: SourceBundle,
        discover: Discover,
    ) -> Self {
        Self {
            id,
            source,
            bundle,
            discover,
        }
    }

    fn root(&self, directory: &Path) -> PathBuf {
        directory.join(self.id)
    }

    fn failure(&self, reason: &str) -> String {
        format!(
            "{} has no valid installed official task catalog ({reason}); run `asb workload install {}`",
            self.id, self.id
        )
    }

    fn discover_installed(&self, directory: &Path) -> Result<Vec<Trial>, String> {
        let root = self.root(directory);
        if fs::read_to_string(root.join("ASB-BUNDLE"))
            .ok()
            .as_deref()
            .map(str::trim)
            != Some(self.bundle.marker)
        {
            return Err(self.failure("pinned source is not installed"));
        }
        let trials = (self.discover)(&root.join("source"))
            .map_err(|reason| self.failure(&format!("source enumeration failed: {reason}")))?;
        validate_trials(&trials).map_err(|reason| self.failure(reason))?;
        Ok(trials)
    }

    fn catalog_path(&self, directory: &Path) -> PathBuf {
        self.root(directory).join("catalog.json")
    }

    fn read_catalog(&self, directory: &Path) -> Result<Vec<Trial>, String> {
        let path = self.catalog_path(directory);
        let metadata =
            fs::symlink_metadata(&path).map_err(|_| self.failure("catalog is missing"))?;
        if metadata.file_type().is_symlink() || metadata.len() > MAX_CATALOG_BYTES {
            return Err(self.failure("catalog is not a bounded regular file"));
        }
        let bytes = fs::read(&path).map_err(|_| self.failure("cannot read catalog"))?;
        let catalog: Catalog =
            serde_json::from_slice(&bytes).map_err(|_| self.failure("catalog has invalid JSON"))?;
        if catalog.schema_version != SCHEMA_VERSION
            || catalog.workload != self.id
            || catalog.marker != self.bundle.marker
        {
            return Err(self.failure("catalog identity does not match its pinned source"));
        }
        let trials = catalog
            .tasks
            .into_iter()
            .map(|task| Trial {
                id: task.id,
                description: task.description,
            })
            .collect::<Vec<_>>();
        validate_trials(&trials).map_err(|reason| self.failure(reason))?;
        Ok(trials)
    }

    fn write_catalog(&self, directory: &Path, trials: Vec<Trial>) -> Result<(), String> {
        validate_trials(&trials).map_err(|reason| self.failure(reason))?;
        let catalog = Catalog {
            schema_version: SCHEMA_VERSION,
            workload: self.id.into(),
            marker: self.bundle.marker.into(),
            tasks: trials
                .into_iter()
                .map(|trial| CatalogTask {
                    id: trial.id,
                    description: trial.description,
                })
                .collect(),
        };
        let bytes = serde_json::to_vec_pretty(&catalog)
            .map_err(|error| self.failure(&format!("cannot serialize catalog: {error}")))?;
        let temporary = self.root(directory).join(".catalog.json.tmp");
        fs::write(&temporary, bytes)
            .map_err(|error| self.failure(&format!("cannot write catalog: {error}")))?;
        fs::rename(&temporary, self.catalog_path(directory))
            .map_err(|error| self.failure(&format!("cannot publish catalog: {error}")))
    }
}

impl WorkloadAdapter for SourceCatalogAdapter {
    fn id(&self) -> &'static str {
        self.id
    }

    fn source(&self) -> Option<&'static str> {
        Some(self.source)
    }

    fn source_bundle(&self) -> Option<SourceBundle> {
        Some(self.bundle)
    }

    fn setup_extension(&self) -> Option<SetupExtension> {
        Some(SetupExtension::trials())
    }

    fn installed_available(&self, directory: &Path) -> Option<bool> {
        Some(self.installed_trials(directory).is_ok())
    }

    fn installed_trials(&self, directory: &Path) -> Result<Vec<Trial>, String> {
        let catalog = self.read_catalog(directory)?;
        let discovered = self.discover_installed(directory)?;
        if catalog.iter().map(|trial| &trial.id).collect::<Vec<_>>()
            != discovered.iter().map(|trial| &trial.id).collect::<Vec<_>>()
        {
            return Err(self.failure("catalog differs from the installed source"));
        }
        Ok(catalog)
    }

    fn finalize_install(&self, directory: &Path) -> Result<(), String> {
        self.write_catalog(directory, self.discover_installed(directory)?)
    }

    fn trial_ids(&self) -> Vec<Trial> {
        Vec::new()
    }

    fn prepare_source(&self, workloads: &Path, workspace: &Path) -> Result<usize, String> {
        super::prepare_external_source(self.id, workloads, workspace)
    }
}

fn validate_trials(trials: &[Trial]) -> Result<(), &'static str> {
    if trials.is_empty() {
        return Err("source contains no official trials");
    }
    if trials.len() > MAX_TASKS {
        return Err("source task population exceeds the supported bound");
    }
    let mut ids = BTreeSet::new();
    for trial in trials {
        if crate::selection_names::validate(&trial.id, "trial").is_err() {
            return Err("source contains an invalid trial identifier");
        }
        if trial.description.trim().is_empty() || !ids.insert(&trial.id) {
            return Err("source contains duplicate or undescribed trials");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn discover(source: &Path) -> Result<Vec<Trial>, String> {
        let mut trials = fs::read_to_string(source.join("trials.txt"))
            .map_err(|error| error.to_string())?
            .lines()
            .map(|id| Trial {
                id: id.into(),
                description: format!("fixture {id}"),
            })
            .collect::<Vec<_>>();
        trials.sort_by(|left, right| left.id.cmp(&right.id));
        Ok(trials)
    }

    const BUNDLE: SourceBundle = SourceBundle {
        archive: "https://invalid.example/fixture.tar.gz",
        marker: "fixture@1",
        prompt: "fixture",
    };
    static ADAPTER: SourceCatalogAdapter = SourceCatalogAdapter::new(
        "fixture",
        "https://invalid.example/readme",
        BUNDLE,
        discover,
    );

    fn fixture() -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "asb-source-catalog-{}",
            crate::workloads::TaskIdentity::local("catalog", None).attempt_id
        ));
        let installed = root.join("fixture");
        fs::create_dir_all(installed.join("source")).unwrap();
        fs::write(installed.join("ASB-BUNDLE"), "fixture@1\n").unwrap();
        fs::write(installed.join("source/trials.txt"), "alpha\nbeta\n").unwrap();
        root
    }

    #[test]
    fn source_catalog_is_generated_and_revalidated_without_network() {
        let root = fixture();
        ADAPTER.finalize_install(&root).unwrap();
        assert_eq!(
            ADAPTER
                .installed_trials(&root)
                .unwrap()
                .into_iter()
                .map(|trial| trial.id)
                .collect::<Vec<_>>(),
            ["alpha", "beta"]
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn substituted_catalog_is_rejected_without_fetching() {
        let root = fixture();
        ADAPTER.finalize_install(&root).unwrap();
        fs::write(
            root.join("fixture/catalog.json"),
            r#"{"schema_version":1,"workload":"fixture","marker":"fixture@1","tasks":[{"id":"wrong","description":"wrong"}]}"#,
        )
        .unwrap();
        assert!(ADAPTER.installed_trials(&root).is_err());
        let _ = fs::remove_dir_all(root);
    }
}
