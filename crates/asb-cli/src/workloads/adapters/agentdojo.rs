//! Source-derived AgentDojo task-class catalog.

use super::{SourceBundle, SourceCatalogAdapter};
use crate::workloads::Trial;
use std::fs;
use std::path::Path;

pub(super) static ADAPTER: SourceCatalogAdapter = SourceCatalogAdapter::new(
    "agentdojo",
    "https://raw.githubusercontent.com/ethz-spylab/agentdojo/089ed468cf3ed0322acc66b0211f26d9d90dbf60/README.md",
    SourceBundle {
        archive: "https://github.com/ethz-spylab/agentdojo/archive/089ed468cf3ed0322acc66b0211f26d9d90dbf60.tar.gz",
        marker: "ethz-spylab/agentdojo@089ed468cf3ed0322acc66b0211f26d9d90dbf60",
        prompt: "Use the selected installed AgentDojo task definition in `external-workload`. Do not download additional data during the run.\n",
    },
    discover,
);

/// AgentDojo assigns numeric runtime IDs while registering source classes.
/// Catalog stable source identities instead, preserving every version, suite,
/// kind, and class without importing or executing the downloaded package.
fn discover(source: &Path) -> Result<Vec<Trial>, String> {
    let suites = source.join("src/agentdojo/default_suites");
    let mut trials = Vec::new();
    for version in read_directories(&suites)? {
        for suite in read_directories(&version.1)? {
            for (kind, filename) in [
                ("user", "user_tasks.py"),
                ("injection", "injection_tasks.py"),
            ] {
                let path = suite.1.join(filename);
                if !path.is_file() {
                    continue;
                }
                for class in task_classes(&path)? {
                    let id = format!("{}--{}--{}--{}", version.0, suite.0, kind, class);
                    trials.push(Trial {
                        description: format!(
                            "AgentDojo {} task {} from {}/{}",
                            kind, class, version.0, suite.0
                        ),
                        id,
                    });
                }
            }
        }
    }
    trials.sort_by(|left, right| left.id.cmp(&right.id));
    trials.dedup_by(|left, right| left.id == right.id);
    Ok(trials)
}

fn read_directories(root: &Path) -> Result<Vec<(String, std::path::PathBuf)>, String> {
    let mut entries = fs::read_dir(root)
        .map_err(|error| format!("cannot read {}: {error}", root.display()))?
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            entry
                .file_type()
                .ok()?
                .is_dir()
                .then_some((name, entry.path()))
        })
        .filter(|(name, _)| !name.starts_with('_') && name != "tools")
        .collect::<Vec<_>>();
    entries.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(entries)
}

fn task_classes(path: &Path) -> Result<Vec<String>, String> {
    let source = fs::read_to_string(path)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    if source.len() > 4 * 1024 * 1024 {
        return Err(format!("task source is too large: {}", path.display()));
    }
    Ok(source
        .lines()
        .filter_map(|line| {
            let class = line.trim_start().strip_prefix("class ")?;
            let name = class.split_once('(')?.0.trim();
            (!name.is_empty()).then(|| name.to_owned())
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_task_classes_are_enumerated_without_importing_python() {
        let root = std::env::temp_dir().join(format!(
            "asb-agentdojo-catalog-{}",
            crate::workloads::TaskIdentity::local("agentdojo", None).attempt_id
        ));
        let suite = root.join("src/agentdojo/default_suites/v1/banking");
        fs::create_dir_all(&suite).unwrap();
        fs::write(
            suite.join("user_tasks.py"),
            "class UserTask0(BankingUserTask):\n    pass\nclass UserTask1(BankingUserTask):\n    pass\n",
        )
        .unwrap();
        fs::write(
            suite.join("injection_tasks.py"),
            "class InjectionTask0(BankingInjectionTask):\n    pass\n",
        )
        .unwrap();
        let trials = discover(&root).unwrap();
        assert_eq!(
            trials.into_iter().map(|trial| trial.id).collect::<Vec<_>>(),
            [
                "v1--banking--injection--InjectionTask0",
                "v1--banking--user--UserTask0",
                "v1--banking--user--UserTask1"
            ]
        );
        let _ = fs::remove_dir_all(root);
    }
}
