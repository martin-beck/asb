//! Source-derived AgentBench task-config catalog.

use super::{SourceBundle, SourceCatalogAdapter};
use crate::workloads::Trial;
use std::fs;
use std::path::Path;

pub(super) static ADAPTER: SourceCatalogAdapter = SourceCatalogAdapter::new(
    "agentbench",
    "https://raw.githubusercontent.com/THUDM/AgentBench/d1e4a10db08c87075c78972e48ecc182be03e2d5/README.md",
    SourceBundle {
        archive: "https://github.com/THUDM/AgentBench/archive/d1e4a10db08c87075c78972e48ecc182be03e2d5.tar.gz",
        marker: "THUDM/AgentBench@d1e4a10db08c87075c78972e48ecc182be03e2d5",
        prompt: "Use the selected installed AgentBench task configuration in `external-workload`. Do not download additional data during the run.\n",
    },
    discover,
);

/// Each checked-in task YAML is an upstream selectable AgentBench task
/// configuration. Read names only; never execute code from the source tree.
fn discover(source: &Path) -> Result<Vec<Trial>, String> {
    let tasks = source.join("configs/tasks");
    let mut ids = fs::read_dir(&tasks)
        .map_err(|error| format!("cannot read {}: {error}", tasks.display()))?
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            let stem = path.file_stem()?.to_str()?;
            (path.is_file()
                && matches!(
                    path.extension().and_then(|extension| extension.to_str()),
                    Some("yaml" | "yml")
                ))
            .then(|| stem.to_owned())
        })
        .collect::<Vec<_>>();
    ids.sort();
    ids.dedup();
    Ok(ids
        .into_iter()
        .map(|id| Trial {
            description: format!("AgentBench official task configuration {id}"),
            id,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_configs_are_enumerated_locally() {
        let root = std::env::temp_dir().join(format!(
            "asb-agentbench-catalog-{}",
            crate::workloads::TaskIdentity::local("agentbench", None).attempt_id
        ));
        let tasks = root.join("configs/tasks");
        fs::create_dir_all(&tasks).unwrap();
        fs::write(tasks.join("webshop.yaml"), "name: webshop\n").unwrap();
        fs::write(tasks.join("dbbench.yml"), "name: dbbench\n").unwrap();
        let trials = discover(&root).unwrap();
        assert_eq!(
            trials.into_iter().map(|trial| trial.id).collect::<Vec<_>>(),
            ["dbbench", "webshop"]
        );
        let _ = fs::remove_dir_all(root);
    }
}
