//! Source-derived catalog for Aider's official Polyglot benchmark.

use super::{SourceBundle, SourceCatalogAdapter};
use crate::workloads::Trial;
use std::fs;
use std::path::Path;

pub(super) static ADAPTER: SourceCatalogAdapter = SourceCatalogAdapter::new(
    "aider-polyglot",
    "https://github.com/Aider-AI/polyglot-benchmark/tree/7e0611e77b54e2dea774cdc0aa00cf9f7ed6144f",
    SourceBundle {
        archive: "https://github.com/Aider-AI/polyglot-benchmark/archive/7e0611e77b54e2dea774cdc0aa00cf9f7ed6144f.tar.gz",
        marker: "Aider-AI/polyglot-benchmark@7e0611e77b54e2dea774cdc0aa00cf9f7ed6144f",
        prompt: "Use the selected installed Aider Polyglot exercise in `external-workload`. Do not download additional data during the run.\n",
    },
    discover,
);

/// A language/exercise pair with an upstream Exercism config is one official
/// Aider Polyglot task.  Only names and checked-in metadata are read.
fn discover(source: &Path) -> Result<Vec<Trial>, String> {
    let mut trials = Vec::new();
    for language in fs::read_dir(source).map_err(|error| error.to_string())? {
        let language = language.map_err(|error| error.to_string())?;
        let kind = language.file_type().map_err(|error| error.to_string())?;
        if !kind.is_dir() || kind.is_symlink() {
            continue;
        }
        let language_name = language.file_name().to_string_lossy().to_string();
        let exercises = language.path().join("exercises/practice");
        let Ok(entries) = fs::read_dir(&exercises) else {
            continue;
        };
        for exercise in entries {
            let exercise = exercise.map_err(|error| error.to_string())?;
            let kind = exercise.file_type().map_err(|error| error.to_string())?;
            if !kind.is_dir() || kind.is_symlink() {
                continue;
            }
            let config = exercise.path().join(".meta/config.json");
            let metadata = fs::symlink_metadata(&config).map_err(|error| error.to_string())?;
            if !metadata.is_file()
                || metadata.file_type().is_symlink()
                || metadata.len() > 128 * 1024
            {
                return Err(format!("invalid exercise metadata {}", config.display()));
            }
            let _: serde_json::Value =
                serde_json::from_slice(&fs::read(&config).map_err(|error| error.to_string())?)
                    .map_err(|error| {
                        format!("invalid exercise metadata {}: {error}", config.display())
                    })?;
            let exercise_name = exercise.file_name().to_string_lossy().to_string();
            trials.push(Trial {
                id: format!("{language_name}--{exercise_name}"),
                description: format!(
                    "Aider Polyglot official {language_name} exercise {exercise_name}"
                ),
            });
        }
    }
    trials.sort_by(|left, right| left.id.cmp(&right.id));
    if trials.len() != 225 {
        return Err(format!(
            "official Aider Polyglot source has {} exercises, expected 225",
            trials.len()
        ));
    }
    Ok(trials)
}

#[cfg(test)]
mod tests {
    use super::super::WorkloadAdapter;
    use super::*;

    #[test]
    fn exercise_metadata_is_enumerated_without_download() {
        let root = std::env::temp_dir().join(format!(
            "asb-aider-polyglot-{}",
            crate::workloads::TaskIdentity::local("aider-polyglot", None).attempt_id
        ));
        for index in 0..225 {
            let exercise = root.join(format!("rust/exercises/practice/task-{index}"));
            fs::create_dir_all(exercise.join(".meta")).unwrap();
            fs::write(exercise.join(".meta/config.json"), "{}").unwrap();
        }
        let trials = discover(&root).unwrap();
        assert_eq!(trials.len(), 225);
        assert_eq!(trials[0].id, "rust--task-0");
        assert_eq!(trials[224].id, "rust--task-99");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn bundle_is_pinned_to_the_official_revision() {
        assert!(
            ADAPTER
                .source()
                .unwrap()
                .contains("7e0611e77b54e2dea774cdc0aa00cf9f7ed6144f")
        );
    }
}
