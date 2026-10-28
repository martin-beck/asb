//! Source-derived CORE-Bench challenge catalog.

use super::{SourceBundle, SourceCatalogAdapter};
use crate::workloads::Trial;
use std::fs;
use std::path::{Path, PathBuf};

const CHALLENGES: &str = "agents/AutoGPT-CORE/benchmark/agbenchmark/challenges";
const MAX_FILES: usize = 10_000;
const MAX_DATA_BYTES: u64 = 128 * 1024;

pub(super) static ADAPTER: SourceCatalogAdapter = SourceCatalogAdapter::new(
    "core-bench",
    "https://raw.githubusercontent.com/siegelz/core-bench/e32a2980e72fe6eb04ee04eb749458f570625663/README.md",
    SourceBundle {
        archive: "https://github.com/siegelz/core-bench/archive/e32a2980e72fe6eb04ee04eb749458f570625663.tar.gz",
        marker: "siegelz/core-bench@e32a2980e72fe6eb04ee04eb749458f570625663",
        prompt: "Use the selected installed CORE-Bench challenge in `external-workload`. Do not download additional data during the run.\n",
    },
    discover,
);

/// A checked-in `data.json` is an official agbenchmark challenge. The stable
/// ASB identifier is its relative challenge path, joined with `--` so it is
/// safe for all selection and TOML interfaces.
fn discover(source: &Path) -> Result<Vec<Trial>, String> {
    let challenges = source.join(CHALLENGES);
    let mut files = Vec::new();
    collect_data_files(&challenges, &mut files)?;
    if files.len() > MAX_FILES {
        return Err("challenge population exceeds the supported bound".into());
    }
    let mut trials = Vec::with_capacity(files.len());
    for path in files {
        let metadata = fs::symlink_metadata(&path).map_err(|error| error.to_string())?;
        if metadata.file_type().is_symlink() || metadata.len() > MAX_DATA_BYTES {
            return Err(format!("invalid challenge data file {}", path.display()));
        }
        let value: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).map_err(|error| error.to_string())?)
                .map_err(|error| format!("invalid challenge data {}: {error}", path.display()))?;
        let relative = path
            .parent()
            .and_then(|parent| parent.strip_prefix(&challenges).ok())
            .ok_or("challenge is outside the expected source tree")?;
        let id = relative
            .components()
            .map(|component| component.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("--");
        let description = value
            .pointer("/info/description")
            .and_then(serde_json::Value::as_str)
            .or_else(|| value.get("name").and_then(serde_json::Value::as_str))
            .unwrap_or("CORE-Bench official challenge")
            .trim()
            .to_owned();
        trials.push(Trial { id, description });
    }
    trials.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(trials)
}

fn collect_data_files(root: &Path, files: &mut Vec<PathBuf>) -> Result<(), String> {
    for entry in
        fs::read_dir(root).map_err(|error| format!("cannot read {}: {error}", root.display()))?
    {
        let entry = entry.map_err(|error| error.to_string())?;
        let path = entry.path();
        let kind = entry.file_type().map_err(|error| error.to_string())?;
        if kind.is_symlink() {
            continue;
        }
        if kind.is_dir() {
            collect_data_files(&path, files)?;
        } else if kind.is_file() && entry.file_name() == "data.json" {
            files.push(path);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checked_in_challenge_metadata_is_enumerated_without_network() {
        let root = std::env::temp_dir().join(format!(
            "asb-core-catalog-{}",
            crate::workloads::TaskIdentity::local("core", None).attempt_id
        ));
        let challenge = root.join(CHALLENGES).join("verticals/code/example");
        fs::create_dir_all(&challenge).unwrap();
        fs::write(
            challenge.join("data.json"),
            r#"{"name":"Example","info":{"description":"A local fixture"}}"#,
        )
        .unwrap();
        let trials = discover(&root).unwrap();
        assert_eq!(trials.len(), 1);
        assert_eq!(trials[0].id, "verticals--code--example");
        assert_eq!(trials[0].description, "A local fixture");
        let _ = fs::remove_dir_all(root);
    }
}
