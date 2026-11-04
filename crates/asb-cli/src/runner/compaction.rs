//! Safe release of an ASB-owned, completed attempt workspace.
//!
//! Project-local tools, downloaded workloads, provider credentials, cassettes,
//! logs and collector output are deliberately outside this boundary.  They are
//! either reusable inputs or durable outcome evidence.  This module only owns
//! the `workspace` sibling of a sealed result's completion directory.

use crate::Error;
use crate::workloads::ResourceDisposition;
use std::path::Path;

/// Compact retention applies only after the workload and proxy have confirmed
/// settlement.  A preserved or absent settlement is deliberately not enough:
/// its workspace may be needed to recover an interrupted attempt.
pub(super) fn eligible(
    enabled: bool,
    completed_successfully: bool,
    cleanup_confirmed: bool,
    disposition: Option<ResourceDisposition>,
) -> bool {
    enabled
        && completed_successfully
        && cleanup_confirmed
        && matches!(
            disposition,
            Some(
                ResourceDisposition::NotRequired
                    | ResourceDisposition::NotCreated
                    | ResourceDisposition::Removed
            )
        )
}

/// Remove the one private task workspace belonging to a sealed completion.
///
/// The completion directory is derived from ASB's published log path.  Making
/// the workspace a literal child of the same result directory prevents this
/// cleanup path from ever accepting a caller-selected project checkout or a
/// shared installation directory.  Symlinks and non-directories fail closed.
pub(super) fn remove_workspace(
    workspace: &Path,
    completion_directory: &Path,
    replica: super::replica::Identity,
) -> Result<(), Error> {
    let Some(result_directory) = completion_directory.parent() else {
        return Err(Error::Input(
            "cannot compact a completion directory without a result directory".into(),
        ));
    };
    let expected_name = if replica.total == 1 {
        "workspace".to_owned()
    } else {
        format!("workspace-concurrency-{}", replica.index)
    };
    if !replica.valid()
        || workspace
            .file_name()
            .is_none_or(|name| name != expected_name.as_str())
        || workspace.parent() != Some(result_directory)
    {
        return Err(Error::Input(
            "refusing to compact a workspace outside its ASB result directory".into(),
        ));
    }
    let result_metadata = std::fs::symlink_metadata(result_directory).map_err(crate::io_error)?;
    if !result_metadata.is_dir() || result_metadata.file_type().is_symlink() {
        return Err(Error::Input(
            "refusing to compact a workspace under a symlinked or invalid result directory".into(),
        ));
    }
    match std::fs::symlink_metadata(workspace) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(crate::io_error(error)),
        Ok(metadata) if !metadata.is_dir() || metadata.file_type().is_symlink() => {
            return Err(Error::Input(
                "refusing to compact a symlinked or non-directory attempt workspace".into(),
            ));
        }
        Ok(_) => {}
    }
    std::fs::remove_dir_all(workspace).map_err(crate::io_error)
}

#[cfg(test)]
mod tests {
    use super::{eligible, remove_workspace};
    use crate::workloads::ResourceDisposition;
    use std::path::Path;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn root(name: &str) -> std::path::PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("asb-compaction-{name}-{stamp}"))
    }

    #[test]
    fn compact_retention_requires_a_confirmed_nonpreserved_settlement() {
        assert!(eligible(
            true,
            true,
            true,
            Some(ResourceDisposition::Removed)
        ));
        assert!(eligible(
            true,
            true,
            true,
            Some(ResourceDisposition::NotRequired)
        ));
        assert!(!eligible(
            true,
            true,
            true,
            Some(ResourceDisposition::StoppedPreserved)
        ));
        assert!(!eligible(
            true,
            true,
            false,
            Some(ResourceDisposition::Removed)
        ));
        assert!(!eligible(
            true,
            false,
            true,
            Some(ResourceDisposition::Removed)
        ));
        assert!(!eligible(
            false,
            true,
            true,
            Some(ResourceDisposition::Removed)
        ));
        assert!(!eligible(true, true, true, None));
    }

    #[test]
    fn removes_only_the_owned_completed_attempt_workspace() {
        let root = root("owned");
        let result = root.join("result-001");
        let workspace = result.join("workspace");
        let measurements = result.join("measurements");
        let completion = result.join(".asb-completions");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&measurements).unwrap();
        std::fs::create_dir_all(&completion).unwrap();
        std::fs::write(workspace.join("unchanged-fixture.txt"), "fixture").unwrap();
        std::fs::write(measurements.join("runtime.json"), "retained").unwrap();

        remove_workspace(&workspace, &completion, Default::default()).unwrap();

        assert!(!workspace.exists());
        assert!(measurements.join("runtime.json").is_file());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn compacts_only_the_matching_concurrent_replica() {
        let root = root("concurrent");
        let completion = root.join(".asb-completions");
        let first = root.join("workspace-concurrency-1");
        let second = root.join("workspace-concurrency-2");
        std::fs::create_dir_all(&completion).unwrap();
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        let replica = super::super::replica::Identity { index: 1, total: 2 };
        assert!(remove_workspace(&second, &completion, replica).is_err());
        assert!(second.exists());
        remove_workspace(&first, &completion, replica).unwrap();
        assert!(!first.exists());
        assert!(second.exists());
        let invalid = super::super::replica::Identity { index: 0, total: 2 };
        assert!(remove_workspace(&second, &completion, invalid).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn refuses_a_caller_selected_or_symlinked_workspace() {
        let root = root("refuse");
        let result = root.join("result-001");
        let completion = result.join(".asb-completions");
        let outside = root.join("caller-checkout/workspace");
        std::fs::create_dir_all(&completion).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        assert!(remove_workspace(&outside, &completion, Default::default()).is_err());
        assert!(outside.exists());

        #[cfg(unix)]
        {
            let workspace = result.join("workspace");
            std::os::unix::fs::symlink(Path::new("../caller-checkout/workspace"), &workspace)
                .unwrap();
            assert!(remove_workspace(&workspace, &completion, Default::default()).is_err());
            assert!(outside.exists());
        }
        std::fs::remove_dir_all(root).unwrap();
    }
}
