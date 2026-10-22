//! Typed local workload discovery; callers own validation and presentation.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DefinitionKind {
    Prompt,
    TrialSet,
}

#[derive(Debug)]
pub(crate) struct Definition {
    pub name: String,
    /// Relative filename, independent of the displayed workload label.
    pub file: PathBuf,
    pub kind: DefinitionKind,
    /// Follows symlinks, matching the setup wizard's existing file filter.
    pub is_file: bool,
}

#[derive(Default)]
pub(crate) struct Catalog {
    pub definitions: Vec<Definition>,
    pub sources: Vec<String>,
}

/// Keep directory-open and individual-entry failures separate so each caller
/// can preserve its existing error policy.
pub(crate) fn paths(directory: &Path) -> io::Result<impl Iterator<Item = io::Result<PathBuf>>> {
    Ok(fs::read_dir(directory)?.map(|entry| entry.map(|entry| entry.path())))
}

pub(crate) fn installable_names() -> impl Iterator<Item = &'static str> {
    super::registered_sources().map(|(id, _)| id)
}

/// A registered installation source, independent of local installation state.
pub(crate) fn is_installable(name: &str) -> bool {
    installable_names().any(|id| id == name)
}

impl Catalog {
    /// Interactive discovery tolerates unreadable directories and entries.
    pub fn best_effort(directory: &Path) -> Self {
        Self::from_paths(
            paths(directory)
                .ok()
                .into_iter()
                .flatten()
                .filter_map(Result::ok),
        )
    }

    pub fn from_paths(paths: impl IntoIterator<Item = PathBuf>) -> Self {
        let mut catalog = Self::default();
        for path in paths {
            let Some(filename) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            let definition = filename
                .strip_suffix(".txt")
                .map(|name| (name, DefinitionKind::Prompt))
                .or_else(|| {
                    filename
                        .strip_suffix(".workload.toml")
                        .map(|name| (name, DefinitionKind::TrialSet))
                });
            if let Some((name, kind)) = definition {
                catalog.definitions.push(Definition {
                    name: name.to_owned(),
                    file: PathBuf::from(filename),
                    kind,
                    is_file: path.is_file(),
                });
            }
            if path.is_dir() && path.join("source").is_dir() {
                catalog.sources.push(filename.to_owned());
            }
        }
        catalog.definitions.sort_by(|a, b| a.file.cmp(&b.file));
        catalog.sources.sort();
        catalog
    }

    /// CLI listing/expansion historically use extension semantics: the dotfile
    /// `.txt` is not a prompt there. The wizard retains its raw suffix identity.
    pub fn prompt_names(&self) -> impl Iterator<Item = &str> {
        self.definitions.iter().filter_map(|entry| {
            (entry.kind == DefinitionKind::Prompt
                && entry.file.extension().is_some_and(|value| value == "txt"))
            .then_some(entry.name.as_str())
        })
    }

    pub fn trial_sets(&self) -> impl Iterator<Item = &Definition> {
        self.definitions
            .iter()
            .filter(|entry| entry.kind == DefinitionKind::TrialSet)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            let root = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../target/workload-catalog")
                .join(super::super::TaskIdentity::local("catalog", None).attempt_id);
            fs::create_dir_all(&root).unwrap();
            Self(root)
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn discovery_retains_raw_file_identities_without_parsing_or_validating() {
        let fixture = Fixture::new();
        for filename in [
            "foo.txt",
            "foo [trial set].txt",
            "foo.workload.toml",
            "all.txt",
            "ignored.md",
            ".txt",
        ] {
            fs::write(fixture.0.join(filename), "not parsed during discovery").unwrap();
        }
        fs::create_dir_all(fixture.0.join("custom/source")).unwrap();
        fs::create_dir_all(fixture.0.join("directory.txt/source")).unwrap();
        let catalog = Catalog::best_effort(&fixture.0);
        let entries = catalog
            .definitions
            .iter()
            .map(|entry| (entry.name.as_str(), entry.kind, entry.is_file))
            .collect::<Vec<_>>();
        assert_eq!(
            entries,
            [
                ("", DefinitionKind::Prompt, true),
                ("all", DefinitionKind::Prompt, true),
                ("directory", DefinitionKind::Prompt, false),
                ("foo [trial set]", DefinitionKind::Prompt, true),
                ("foo", DefinitionKind::Prompt, true),
                ("foo", DefinitionKind::TrialSet, true),
            ]
        );
        assert_eq!(catalog.sources, ["custom", "directory.txt"]);
        assert_eq!(
            catalog.prompt_names().collect::<Vec<_>>(),
            ["all", "directory", "foo [trial set]", "foo"]
        );
        let sets = catalog.trial_sets().collect::<Vec<_>>();
        assert_eq!(sets.len(), 1);
        assert_eq!(sets[0].file, Path::new("foo.workload.toml"));
    }

    #[test]
    fn directory_failures_remain_available_to_strict_callers() {
        let fixture = Fixture::new();
        let missing = fixture.0.join("missing");
        assert_eq!(
            paths(&missing).err().unwrap().kind(),
            io::ErrorKind::NotFound
        );
        fs::write(fixture.0.join("file"), "fixture").unwrap();
        assert!(paths(&fixture.0.join("file")).is_err());
        for path in [missing, fixture.0.join("file")] {
            let catalog = Catalog::best_effort(&path);
            assert!(catalog.definitions.is_empty());
            assert!(catalog.sources.is_empty());
        }
    }

    #[cfg(unix)]
    #[test]
    fn discovery_follows_symlinks_and_ignores_non_utf8_names() {
        use std::ffi::OsString;
        use std::os::unix::{ffi::OsStringExt, fs::symlink};

        let fixture = Fixture::new();
        fs::write(fixture.0.join("original"), "prompt").unwrap();
        symlink("original", fixture.0.join("linked.txt")).unwrap();
        symlink("missing", fixture.0.join("dangling.txt")).unwrap();
        fs::create_dir_all(fixture.0.join("original-source/source")).unwrap();
        symlink("original-source", fixture.0.join("linked-source")).unwrap();
        let non_utf8 = OsString::from_vec(b"invalid-\xff.txt".to_vec());
        fs::write(fixture.0.join(non_utf8), "prompt").unwrap();
        let catalog = Catalog::best_effort(&fixture.0);
        assert_eq!(
            catalog.prompt_names().collect::<Vec<_>>(),
            ["dangling", "linked"]
        );
        assert!(!catalog.definitions[0].is_file);
        assert!(catalog.definitions[1].is_file);
        assert_eq!(catalog.sources, ["linked-source", "original-source"]);
    }
}
