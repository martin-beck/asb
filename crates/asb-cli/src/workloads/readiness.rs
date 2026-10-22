//! Shared installation inspection, independent of frontend display policy.
//!
//! Registered adapters retain their own validation. Generic installation checks
//! observe local files; they do not validate prompt contents, pinned bundle
//! revisions or execution prerequisites. Prompt validation remains in `load`.

use super::{catalog, installed_available, source_bundle};
use std::path::Path;

/// Source material that the installer can validate and repair without downloading
/// again. Derived prompt/fixture files and pinned revision validation are separate.
pub(crate) fn bundle_material_present(name: &str, directory: &Path) -> bool {
    let bundle = directory.join(name);
    bundle.join("ASB-BUNDLE").is_file() && bundle.join("source").is_dir()
}

pub(crate) fn installed(name: &str, directory: &Path) -> bool {
    if let Some(available) = installed_available(name, directory) {
        return available;
    }
    if source_bundle(name).is_some() {
        bundle_material_present(name, directory)
            && directory
                .join(name)
                .join("asb-trial/ASB-ADAPTER-FIXTURE")
                .is_file()
            && directory.join(format!("{name}.txt")).is_file()
    } else {
        directory.join(format!("{name}.txt")).is_file()
            && (!catalog::is_installable(name)
                || directory
                    .join(name)
                    .join("asb-trial/ASB-ADAPTER-FIXTURE")
                    .is_file())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workloads::{TaskIdentity, load};
    use std::fs;
    use std::path::PathBuf;

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            let root = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../target/workload-readiness")
                .join(TaskIdentity::local("readiness", None).attempt_id);
            fs::create_dir_all(&root).unwrap();
            Self(root)
        }

        fn write(&self, path: &str, contents: impl AsRef<[u8]>) {
            let path = self.0.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, contents).unwrap();
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn installation_presence_is_independent_of_prompt_validation() {
        let fixture = Fixture::new();
        assert!(!installed("custom", &fixture.0));
        fixture.write("custom.txt", "Local task.");
        assert!(installed("custom", &fixture.0));
        assert!(load("custom", &fixture.0).is_ok());
        for (name, contents) in [
            ("empty", " \n".to_owned()),
            ("oversized", "x".repeat(64 * 1024 + 1)),
            ("invalid name", "Local task.".to_owned()),
        ] {
            fixture.write(&format!("{name}.txt"), contents);
            assert!(installed(name, &fixture.0), "{name}");
            assert!(load(name, &fixture.0).is_err(), "{name}");
        }
        assert!(!installed("original.custom", &fixture.0));
        assert!(load("original.custom", &fixture.0).is_ok());
        fs::remove_file(fixture.0.join("custom.txt")).unwrap();
        assert!(!installed("custom", &fixture.0));
    }

    #[test]
    fn registered_workloads_require_their_fixture_and_bundle_components() {
        let fixture = Fixture::new();
        fixture.write("agentbench.txt", "Fixture task.");
        assert!(!installed("agentbench", &fixture.0));
        fixture.write("agentbench/asb-trial/ASB-ADAPTER-FIXTURE", "fixture");
        assert!(installed("agentbench", &fixture.0));

        fixture.write("terminal-bench.txt", "Fixture task.");
        fixture.write("terminal-bench/ASB-BUNDLE", "fixture");
        fs::create_dir_all(fixture.0.join("terminal-bench/source")).unwrap();
        assert!(bundle_material_present("terminal-bench", &fixture.0));
        assert!(!installed("terminal-bench", &fixture.0));
        fixture.write("terminal-bench/asb-trial/ASB-ADAPTER-FIXTURE", "fixture");
        assert!(installed("terminal-bench", &fixture.0));
        fs::remove_dir(fixture.0.join("terminal-bench/source")).unwrap();
        assert!(!bundle_material_present("terminal-bench", &fixture.0));
        assert!(!installed("terminal-bench", &fixture.0));
        fs::create_dir(fixture.0.join("terminal-bench/source")).unwrap();
        fs::remove_file(fixture.0.join("terminal-bench/ASB-BUNDLE")).unwrap();
        assert!(!installed("terminal-bench", &fixture.0));
    }

    #[test]
    fn adapter_validation_cannot_fall_back_to_generic_presence_checks() {
        let fixture = Fixture::new();
        for name in ["swe-bench", "terminal-bench-1.0"] {
            fixture.write(&format!("{name}.txt"), "A prompt alone is not a dataset.");
            fixture.write(&format!("{name}/ASB-BUNDLE"), "fixture");
            fixture.write(&format!("{name}/asb-trial/ASB-ADAPTER-FIXTURE"), "fixture");
            fixture.write(&format!("{name}/catalog.json"), "{}");
            fs::create_dir_all(fixture.0.join(name).join("source")).unwrap();
            assert!(!installed(name, &fixture.0), "{name}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn installation_presence_follows_prompt_symlinks_without_loading_contents() {
        use std::os::unix::fs::symlink;

        let fixture = Fixture::new();
        fixture.write("target", "");
        symlink("target", fixture.0.join("custom.txt")).unwrap();
        assert!(installed("custom", &fixture.0));
        assert!(load("custom", &fixture.0).is_err());
        fs::remove_file(fixture.0.join("target")).unwrap();
        assert!(!installed("custom", &fixture.0));
    }
}
