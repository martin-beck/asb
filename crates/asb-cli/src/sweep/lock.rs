//! Sweep-specific paths and diagnostics over shared output ownership.
use crate::Error;
use std::path::{Path, PathBuf};

pub(crate) struct OutputLock {
    _owner: crate::output_lock::OutputLock,
    _ancestors: Vec<crate::output_lock::OutputLock>,
}

impl OutputLock {
    pub(crate) fn acquire(parent: &Path, stem: &str) -> Result<Self, Error> {
        // A sweep can itself be inside a prior sweep's result tree. Take the
        // enclosing owners from outermost to innermost before taking its own
        // owner, so comparisons of any nested input and result rotation are
        // mutually exclusive. A failed nonblocking acquisition drops all prior
        // owners; no lock is held while waiting for another owner.
        let parent = canonical_directory(parent)?;
        let mut ancestors = enclosing_owners(&parent);
        ancestors.push((parent, stem.to_owned()));
        Self::acquire_owners(ancestors)
    }

    fn acquire_owners(mut candidates: Vec<(PathBuf, String)>) -> Result<Self, Error> {
        // Canonical paths deduplicate aliases. Nonblocking acquisition and the
        // same outermost-first total ordering avoid lock-order cycles even if
        // a source path traverses several nested or aliased result trees.
        candidates.sort_by(|a, b| {
            a.0.components()
                .count()
                .cmp(&b.0.components().count())
                .then_with(|| a.cmp(b))
        });
        candidates.dedup();
        let mut owners = Vec::new();
        for (directory, ancestor) in candidates {
            owners.push(Self::acquire_one(&directory, &ancestor)?);
        }
        let owner = owners
            .pop()
            .expect("at least one comparison or sweep owner");
        Ok(Self {
            _owner: owner,
            _ancestors: owners,
        })
    }

    fn acquire_one(parent: &Path, stem: &str) -> Result<crate::output_lock::OutputLock, Error> {
        let path = parent.join(format!(".{stem}-asb-sweep.lock"));
        let busy = format!(
            "another sweep owns the output for `{stem}`, or a comparison is using it; wait for it to finish or use a different manifest filename"
        );
        crate::output_lock::OutputLock::acquire(&path, &busy).map_err(Error::Run)
    }

    /// Coordinate reads of a sweep manifest (or one of its campaign plans)
    /// with the same owner that can rotate those source results.
    pub(crate) fn for_comparison(path: &Path) -> Result<Option<Self>, Error> {
        let parent = canonical_directory(path.parent().unwrap_or_else(|| Path::new(".")))?;
        let mut candidates = enclosing_owners(&parent);
        let parent_is_nested = !candidates.is_empty();
        if let Ok(target) = std::fs::canonicalize(path) {
            candidates.extend(enclosing_owners(&target));
        }
        // A lexical manifest outside a result tree names its own result
        // population even when the manifest itself is a symlink. Preserve
        // that naming while also guarding any result tree the target lies in.
        if !parent_is_nested && path.file_name().is_none_or(|name| name != "plan.json") {
            let stem = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .filter(|stem| !stem.is_empty())
                .unwrap_or("sweep");
            candidates.push((parent, stem.to_owned()));
        }
        if candidates.is_empty() {
            Ok(None)
        } else {
            Self::acquire_owners(candidates).map(Some)
        }
    }
}

fn canonical_directory(path: &Path) -> Result<PathBuf, Error> {
    let path = if path.as_os_str().is_empty() {
        Path::new(".")
    } else {
        path
    };
    std::fs::canonicalize(path).map_err(|error| {
        Error::Io(format!(
            "cannot resolve sweep ownership directory {}: {error}",
            path.display()
        ))
    })
}

fn enclosing_owners(path: &Path) -> Vec<(PathBuf, String)> {
    path.ancestors()
        .filter_map(|ancestor| {
            let stem = ancestor.file_name()?.to_str()?.strip_suffix("-results")?;
            if stem.is_empty() {
                return None;
            }
            Some((ancestor.parent()?.to_path_buf(), stem.to_owned()))
        })
        .collect()
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_child() {
        let Some(root) = std::env::var_os("ASB_SWEEP_LOCK_TEST_PARENT") else {
            return;
        };
        let root = std::path::PathBuf::from(root);
        let _owner = OutputLock::acquire(&root, "sweep").unwrap();
        std::fs::write(root.join("ready"), "ready").unwrap();
        let mut byte = [0];
        let _ = std::io::Read::read(&mut std::io::stdin(), &mut byte);
    }

    #[test]
    fn abrupt_writer_exit_releases_only_its_output_lock() {
        let root = std::env::temp_dir()
            .join(crate::workloads::TaskIdentity::local("sweep-lock-process", None).attempt_id);
        std::fs::create_dir_all(&root).unwrap();
        let unrelated = OutputLock::acquire(&root, "other").unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "sweep::lock::tests::lock_child", "--nocapture"])
            .env("ASB_SWEEP_LOCK_TEST_PARENT", &root)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !root.join("ready").is_file() && std::time::Instant::now() < deadline {
            assert!(
                child.try_wait().unwrap().is_none(),
                "lock fixture exited early"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        if !root.join("ready").is_file() {
            let _ = child.kill();
            let _ = child.wait();
            panic!("lock fixture did not become ready");
        }
        assert!(OutputLock::acquire(&root, "sweep").is_err());
        child.kill().unwrap();
        child.wait().unwrap();
        let successor = OutputLock::acquire(&root, "sweep").unwrap();
        assert!(OutputLock::acquire(&root, "other").is_err());
        drop((successor, unrelated));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn sweep_output_ownership_excludes_competitors_and_releases_on_drop() {
        let root = std::env::temp_dir()
            .join(crate::workloads::TaskIdentity::local("sweep-lock", None).attempt_id);
        std::fs::create_dir_all(root.join("other")).unwrap();
        let path = root.join(".sweep-asb-sweep.lock");
        std::fs::write(&path, "existing contents must not be truncated").unwrap();
        let owner = OutputLock::acquire(&root, "sweep").unwrap();
        let inherited_descriptor = owner._owner.cloned_descriptor();
        let conflict = OutputLock::acquire(&root, "sweep").err().unwrap();
        assert!(conflict.to_string().contains("another sweep owns"));
        let unrelated_stem = OutputLock::acquire(&root, "different").unwrap();
        let unrelated_parent = OutputLock::acquire(&root.join("other"), "sweep").unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "existing contents must not be truncated"
        );
        drop((owner, unrelated_stem, unrelated_parent));
        let successor = OutputLock::acquire(&root, "sweep").unwrap();
        drop(inherited_descriptor);
        drop(successor);
        assert!(path.is_file(), "do not unlink and split future lock owners");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn nested_sweep_owns_enclosing_results_before_its_own_output() {
        let root = std::env::temp_dir()
            .join(crate::workloads::TaskIdentity::local("nested-sweep-lock", None).attempt_id);
        let nested = root.join("outer-results/000-benchmark");
        std::fs::create_dir_all(&nested).unwrap();
        let outer = OutputLock::acquire(&root, "outer").unwrap();
        assert!(OutputLock::acquire(&nested, "inner").is_err());
        assert!(!nested.join(".inner-asb-sweep.lock").exists());
        drop(outer);
        let inner = OutputLock::acquire(&nested, "inner").unwrap();
        assert!(OutputLock::acquire(&root, "outer").is_err());
        assert!(OutputLock::for_comparison(&nested.join("benchmark.toml")).is_err());
        drop(inner);
        assert!(OutputLock::acquire(&root, "outer").is_ok());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn nested_sweep_directory_symlink_still_owns_enclosing_results() {
        let root = std::env::temp_dir()
            .join(crate::workloads::TaskIdentity::local("nested-sweep-alias", None).attempt_id);
        let nested = root.join("outer-results/000-benchmark");
        std::fs::create_dir_all(&nested).unwrap();
        let alias = root.join("alias");
        std::os::unix::fs::symlink(&nested, &alias).unwrap();
        let owner = OutputLock::acquire(&root, "outer").unwrap();
        assert!(OutputLock::acquire(&alias, "inner").is_err());
        drop(owner);
        let owner = OutputLock::acquire(&alias, "inner").unwrap();
        assert!(OutputLock::acquire(&root, "outer").is_err());
        drop(owner);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn sweep_output_lock_rejects_symlinks_and_non_regular_files() {
        let root = std::env::temp_dir()
            .join(crate::workloads::TaskIdentity::local("sweep-lock-path", None).attempt_id);
        std::fs::create_dir_all(&root).unwrap();
        let target = root.join("untouched");
        std::fs::write(&target, "keep").unwrap();
        std::os::unix::fs::symlink(&target, root.join(".linked-asb-sweep.lock")).unwrap();
        std::fs::create_dir(root.join(".directory-asb-sweep.lock")).unwrap();
        for stem in ["linked", "directory"] {
            assert!(OutputLock::acquire(&root, stem).is_err());
        }
        assert_eq!(std::fs::read_to_string(target).unwrap(), "keep");
        std::fs::remove_dir_all(root).unwrap();
    }
}
