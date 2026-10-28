//! Source-derived SWE-Lancer task catalog.

use super::{SourceBundle, SourceCatalogAdapter};
use crate::workloads::Trial;
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

const TASKS: &str = "project/swelancer/all_swelancer_tasks.csv";

pub(super) static ADAPTER: SourceCatalogAdapter = SourceCatalogAdapter::new(
    "swe-lancer",
    "https://raw.githubusercontent.com/openai/frontier-evals/51052cede8cc608f95bb00346635e03759013e5a/README.md",
    SourceBundle {
        archive: "https://github.com/openai/frontier-evals/archive/51052cede8cc608f95bb00346635e03759013e5a.tar.gz",
        marker: "openai/frontier-evals@51052cede8cc608f95bb00346635e03759013e5a",
        prompt: "Use the selected installed SWE-Lancer task in `external-workload`. Do not download additional data during the run.\n",
    },
    discover,
);

/// The official CSV's first field is `question_id`. It is a simple identifier
/// before every quoted, free-text field, so no CSV parser needs to process the
/// task descriptions merely to enumerate the canonical population.
fn discover(source: &Path) -> Result<Vec<Trial>, String> {
    let path = source.join(TASKS);
    let metadata = fs::symlink_metadata(&path).map_err(|error| error.to_string())?;
    if metadata.file_type().is_symlink() || metadata.len() > 64 * 1024 * 1024 {
        return Err(format!("invalid SWE-Lancer task list {}", path.display()));
    }
    let text = fs::read_to_string(&path).map_err(|error| error.to_string())?;
    let mut lines = text.lines();
    if lines.next().and_then(|line| line.split(',').next()) != Some("question_id") {
        return Err("SWE-Lancer task list has no question_id header".into());
    }
    let mut ids = BTreeSet::new();
    for line in lines {
        let id = line.split(',').next().unwrap_or_default().trim();
        if id.is_empty() {
            continue;
        }
        ids.insert(id.to_owned());
    }
    Ok(ids
        .into_iter()
        .map(|id| Trial {
            description: format!("SWE-Lancer official task {id}"),
            id,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn official_task_ids_are_enumerated_without_parsing_text_fields() {
        let root = std::env::temp_dir().join(format!(
            "asb-swe-lancer-catalog-{}",
            crate::workloads::TaskIdentity::local("swe-lancer", None).attempt_id
        ));
        let list = root.join(TASKS);
        fs::create_dir_all(list.parent().unwrap()).unwrap();
        fs::write(
            &list,
            "question_id,title,description\nissue_2,Title,\"text, with commas\"\nissue_1,Other,plain\n",
        )
        .unwrap();
        assert_eq!(
            discover(&root)
                .unwrap()
                .into_iter()
                .map(|trial| trial.id)
                .collect::<Vec<_>>(),
            ["issue_1", "issue_2"]
        );
        let _ = fs::remove_dir_all(root);
    }
}
