//! Source-derived SWE-Efficiency lite instance catalog.

use super::{SourceBundle, SourceCatalogAdapter};
use crate::workloads::Trial;
use std::fs;
use std::path::Path;

const INSTANCE_LIST: &str = "analysis/adhoc/make_lite/lite_selection/selected_instance_ids.json";

pub(super) static ADAPTER: SourceCatalogAdapter = SourceCatalogAdapter::new(
    "swe-efficiency",
    "https://raw.githubusercontent.com/swefficiency/swefficiency/12d32a2d6800824a7d84bdb6797b5708e7b7957f/README.md",
    SourceBundle {
        archive: "https://github.com/swefficiency/swefficiency/archive/12d32a2d6800824a7d84bdb6797b5708e7b7957f.tar.gz",
        marker: "swefficiency/swefficiency@12d32a2d6800824a7d84bdb6797b5708e7b7957f",
        prompt: "Use the selected installed SWE-Efficiency instance in `external-workload`. Do not download additional data during the run.\n",
    },
    discover,
);

fn discover(source: &Path) -> Result<Vec<Trial>, String> {
    let path = source.join(INSTANCE_LIST);
    let metadata = fs::symlink_metadata(&path).map_err(|error| error.to_string())?;
    if metadata.file_type().is_symlink() || metadata.len() > 4 * 1024 * 1024 {
        return Err(format!(
            "invalid SWE-Efficiency instance list {}",
            path.display()
        ));
    }
    let mut ids: Vec<String> =
        serde_json::from_slice(&fs::read(&path).map_err(|error| error.to_string())?)
            .map_err(|error| format!("invalid SWE-Efficiency instance list: {error}"))?;
    ids.sort();
    ids.dedup();
    Ok(ids
        .into_iter()
        .map(|id| Trial {
            description: format!("SWE-Efficiency official selected instance {id}"),
            id,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn official_instance_list_is_enumerated_without_network() {
        let root = std::env::temp_dir().join(format!(
            "asb-swe-efficiency-catalog-{}",
            crate::workloads::TaskIdentity::local("swe-efficiency", None).attempt_id
        ));
        let list = root.join(INSTANCE_LIST);
        fs::create_dir_all(list.parent().unwrap()).unwrap();
        fs::write(&list, r#"["org__project-2", "org__project-1"]"#).unwrap();
        assert_eq!(
            discover(&root)
                .unwrap()
                .into_iter()
                .map(|trial| trial.id)
                .collect::<Vec<_>>(),
            ["org__project-1", "org__project-2"]
        );
        let _ = fs::remove_dir_all(root);
    }
}
