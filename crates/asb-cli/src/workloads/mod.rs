//! Workload prompts loaded from the adjacent `workloads` directory.

use crate::suggestions;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

mod adapters;

const MAX_PROMPT_BYTES: u64 = 64 * 1024;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TrialSet {
    pub workload: String,
    pub trials: Vec<Trial>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Trial {
    pub id: String,
    #[serde(default)]
    pub description: String,
}

struct Fixture {
    name: &'static str,
    files: &'static [(&'static str, &'static str)],
}

const BUILTIN: &[(&str, &str)] = &[
    (
        "bug-fix",
        include_str!("../../assets/workloads/bug-fix.txt"),
    ),
    (
        "build-repair",
        include_str!("../../assets/workloads/build-repair.txt"),
    ),
    (
        "dependency-migration",
        include_str!("../../assets/workloads/dependency-migration.txt"),
    ),
    (
        "feature-addition",
        include_str!("../../assets/workloads/feature-addition.txt"),
    ),
    (
        "refactoring",
        include_str!("../../assets/workloads/refactoring.txt"),
    ),
    (
        "repository-navigation",
        include_str!("../../assets/workloads/repository-navigation.txt"),
    ),
    (
        "test-generation",
        include_str!("../../assets/workloads/test-generation.txt"),
    ),
];

const FIXTURES: &[Fixture] = &[
    Fixture {
        name: "bug-fix",
        files: &[
            (
                "README.md",
                include_str!("../../assets/workloads/fixtures/bug-fix/README.md"),
            ),
            (
                "src/parser.py",
                include_str!("../../assets/workloads/fixtures/bug-fix/src/parser.py"),
            ),
            (
                "tests/test_parser.py",
                include_str!("../../assets/workloads/fixtures/bug-fix/tests/test_parser.py"),
            ),
        ],
    },
    Fixture {
        name: "build-repair",
        files: &[
            (
                "README.md",
                include_str!("../../assets/workloads/fixtures/build-repair/README.md"),
            ),
            (
                "src/app.py",
                include_str!("../../assets/workloads/fixtures/build-repair/src/app.py"),
            ),
            (
                "tests/test_app.py",
                include_str!("../../assets/workloads/fixtures/build-repair/tests/test_app.py"),
            ),
            (
                "build.sh",
                include_str!("../../assets/workloads/fixtures/build-repair/build.sh"),
            ),
        ],
    },
    Fixture {
        name: "dependency-migration",
        files: &[
            (
                "README.md",
                include_str!("../../assets/workloads/fixtures/dependency-migration/README.md"),
            ),
            (
                "requirements.txt",
                include_str!(
                    "../../assets/workloads/fixtures/dependency-migration/requirements.txt"
                ),
            ),
            (
                "src/client.py",
                include_str!("../../assets/workloads/fixtures/dependency-migration/src/client.py"),
            ),
            (
                "tests/test_client.py",
                include_str!(
                    "../../assets/workloads/fixtures/dependency-migration/tests/test_client.py"
                ),
            ),
        ],
    },
    Fixture {
        name: "feature-addition",
        files: &[
            (
                "README.md",
                include_str!("../../assets/workloads/fixtures/feature-addition/README.md"),
            ),
            (
                "src/formatter.py",
                include_str!("../../assets/workloads/fixtures/feature-addition/src/formatter.py"),
            ),
            (
                "tests/test_formatter.py",
                include_str!(
                    "../../assets/workloads/fixtures/feature-addition/tests/test_formatter.py"
                ),
            ),
        ],
    },
    Fixture {
        name: "refactoring",
        files: &[
            (
                "README.md",
                include_str!("../../assets/workloads/fixtures/refactoring/README.md"),
            ),
            (
                "src/labels.py",
                include_str!("../../assets/workloads/fixtures/refactoring/src/labels.py"),
            ),
            (
                "tests/test_labels.py",
                include_str!("../../assets/workloads/fixtures/refactoring/tests/test_labels.py"),
            ),
        ],
    },
    Fixture {
        name: "repository-navigation",
        files: &[
            (
                "README.md",
                include_str!("../../assets/workloads/fixtures/repository-navigation/README.md"),
            ),
            (
                "src/cli.py",
                include_str!("../../assets/workloads/fixtures/repository-navigation/src/cli.py"),
            ),
            (
                "src/registry.py",
                include_str!(
                    "../../assets/workloads/fixtures/repository-navigation/src/registry.py"
                ),
            ),
            (
                "tests/test_cli.py",
                include_str!(
                    "../../assets/workloads/fixtures/repository-navigation/tests/test_cli.py"
                ),
            ),
        ],
    },
    Fixture {
        name: "test-generation",
        files: &[
            (
                "README.md",
                include_str!("../../assets/workloads/fixtures/test-generation/README.md"),
            ),
            (
                "src/temperature.py",
                include_str!("../../assets/workloads/fixtures/test-generation/src/temperature.py"),
            ),
            (
                "tests/test_temperature.py",
                include_str!(
                    "../../assets/workloads/fixtures/test-generation/tests/test_temperature.py"
                ),
            ),
        ],
    },
];

/// Populate a new project with the prompts shipped in the ASB binary.
pub(crate) fn populate(directory: &Path) -> Result<usize, std::io::Error> {
    adapters::populate_prompts(directory)
}

/// Generic prompt materialization used by the prompt-family adapter.
pub(super) fn populate_builtin_prompts(directory: &Path) -> Result<usize, std::io::Error> {
    let mut added = 0;
    for (name, prompt) in BUILTIN {
        let path = directory.join(format!("{name}.txt"));
        if !path.exists() {
            fs::write(path, prompt)?;
            added += 1;
        }
    }
    Ok(added)
}

/// Copy all bundled fixture templates into a project's managed fixture area.
pub(crate) fn populate_fixtures(directory: &Path) -> Result<usize, std::io::Error> {
    adapters::populate_fixtures(directory)
}

/// Generic fixture materialization used by the prompt-family adapter.
pub(super) fn populate_builtin_fixtures(directory: &Path) -> Result<usize, std::io::Error> {
    let mut added = 0;
    for fixture in FIXTURES {
        for (relative, contents) in fixture.files {
            let path = directory.join(fixture.name).join(relative);
            if !path.exists() {
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::write(&path, contents)?;
                if relative.ends_with(".sh") {
                    set_executable_io(&path)?;
                }
                added += 1;
            }
        }
    }
    Ok(added)
}

/// Materialize the selected fixture into the run workspace without overwriting edits.
pub(crate) fn prepare_fixture(name: &str, directory: &Path) -> Result<usize, String> {
    adapters::find(name).map_or(Ok(0), |adapter| adapter.prepare_fixture(name, directory))
}

pub(super) fn is_builtin_prompt(name: &str) -> bool {
    BUILTIN.iter().any(|(id, _)| *id == name)
}

/// Generic fixture setup used by the prompt-family adapter.
pub(super) fn prepare_builtin_fixture(name: &str, directory: &Path) -> Result<usize, String> {
    let Some(fixture) = FIXTURES.iter().find(|fixture| fixture.name == name) else {
        return Ok(0);
    };
    let mut added = 0;
    for (relative, contents) in fixture.files {
        let path = directory.join(relative);
        if !path.exists() {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).map_err(|error| {
                    format!(
                        "cannot create fixture directory {}: {error}",
                        parent.display()
                    )
                })?;
            }
            fs::write(&path, contents)
                .map_err(|error| format!("cannot write fixture {}: {error}", path.display()))?;
            if relative.ends_with(".sh") {
                set_executable(&path)?;
            }
            added += 1;
        }
    }
    Ok(added)
}

/// Copy an installed external workload source into the run workspace.
pub(crate) fn prepare_external(
    name: &str,
    workload_directory: &Path,
    workspace: &Path,
) -> Result<usize, String> {
    if let Some(adapter) = adapters::find(name) {
        return adapter.prepare_source(workload_directory, workspace);
    }
    Ok(0)
}

/// Generic source materialization shared by external-suite adapters.
pub(crate) fn prepare_external_source(
    name: &str,
    workload_directory: &Path,
    workspace: &Path,
) -> Result<usize, String> {
    let source = workload_directory.join(name).join("source");
    let mut added = if source.is_dir() {
        copy_tree(&source, &workspace.join("external-workload"))?
    } else {
        0
    };
    let fixture = workload_directory.join(name).join("asb-trial");
    if fixture.is_dir() {
        added += copy_tree(
            &fixture,
            &workspace.join("external-workload").join("asb-trial"),
        )?;
    }
    Ok(added)
}

/// Materialize a deterministic, locally gradable adapter trial beside the
/// pinned upstream reference.  The reference remains available for context,
/// while this small case makes every external alias executable offline.
pub(crate) fn materialize_external_adapter(
    name: &str,
    directory: &Path,
) -> Result<(), std::io::Error> {
    if adapters::find(name).is_none() || is_builtin_prompt(name) {
        return Ok(());
    }
    let root = directory.join(name).join("asb-trial");
    let trial_label = adapter_trial_label(name);
    let files = [
        (
            "README.md",
            "# ASB adapter trial\n\nFix `src/task.py` so the supplied unit test passes. Preserve the suite-specific `suite_label()` value.\n",
        ),
        (
            "src/task.py",
            "def normalize(value: str) -> str:\n    return value.strip().replace(' ', '_')\n\ndef suite_label() -> str:\n    return \"__ASB_TRIAL_LABEL__\"\n",
        ),
        (
            "tests/test_task.py",
            "import unittest\nfrom src.task import normalize\n\nclass TaskTests(unittest.TestCase):\n    def test_normalize(self):\n        self.assertEqual(normalize(' Hello World '), 'hello-world')\n\nif __name__ == '__main__':\n    unittest.main()\n",
        ),
        ("src/__init__.py", ""),
        ("tests/__init__.py", ""),
    ];
    for (relative, contents) in files {
        let contents = contents.replace("__ASB_TRIAL_LABEL__", &trial_label);
        let path = root.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, contents)?;
    }
    fs::write(
        root.join("ASB-ADAPTER-FIXTURE"),
        format!("{name}/asb-adapter-trial-v1\n"),
    )?;
    fs::write(
        directory.join(format!("{name}.txt")),
        format!(
            "External workload adapter trial `{name}`. In `external-workload/asb-trial`, fix `src/task.py` so `normalize` returns lowercase hyphenated text and preserve suite_label()=`{trial_label}`. Do not edit the tests.\n"
        ),
    )?;
    Ok(())
}

fn adapter_trial_label(name: &str) -> String {
    name.replace('-', "_")
}

/// Return the kind of deterministic evaluation used for a workload.
///
/// External suite names are references to upstream suites, not claims that a
/// complete upstream benchmark has been executed.  Until an upstream suite
/// adapter is selected and installed, ASB runs the small adapter-smoke trial
/// that is materialized beside that reference.
pub(crate) fn evaluation_kind(name: &str, _workspace: &Path) -> &'static str {
    if name == "repository-navigation" {
        return "response-review";
    }
    if source_expectation(name).is_some() {
        return "bundled-fixture";
    }
    if adapters::find(name).is_some() {
        "adapter-smoke"
    } else {
        "ungraded"
    }
}

struct SourceExpectation {
    files: &'static [FileExpectation],
}

struct FileExpectation {
    relative: &'static str,
    /// Complete evaluator-owned solutions accepted for this fixed fixture.
    ///
    /// The grader deliberately compares the whole bounded artifact instead of
    /// looking for a few lexical markers.  That prevents dead, unreachable,
    /// or unused code from being mistaken for a completed task.
    accepted: &'static [&'static str],
}

fn source_expectation(name: &str) -> Option<SourceExpectation> {
    Some(match name {
        "bug-fix" => SourceExpectation {
            files: &[FileExpectation {
                relative: "src/parser.py",
                accepted: &[
                    "def parse_path(value: str) -> list[str]:\n    return [segment for segment in value.split(\"/\") if segment]\n",
                    "def parse_path(value: str) -> list[str]:\n    return [segment for segment in value.split('/') if segment]\n",
                    "def parse_path(value: str) -> list[str]:\n    \"\"\"Split a slash-separated path into non-empty segments.\"\"\"\n    return [segment for segment in value.split(\"/\") if segment]\n",
                ],
            }],
        },
        "build-repair" => SourceExpectation {
            files: &[FileExpectation {
                relative: "build.sh",
                accepted: &[
                    "#!/bin/sh\nset -eu\npython3 -m compileall -q src tests\npython3 -m unittest discover -s tests\npython3 src/app.py\n",
                ],
            }],
        },
        "dependency-migration" => SourceExpectation {
            files: &[FileExpectation {
                relative: "requirements.txt",
                accepted: &["example-client==2.0\n"],
            }],
        },
        "feature-addition" => SourceExpectation {
            files: &[FileExpectation {
                relative: "src/formatter.py",
                accepted: &[
                    "def format_items(items: list[str], reverse: bool = False) -> str:\n    ordered = list(reversed(items)) if reverse else items\n    return \", \".join(ordered)\n",
                    "def format_items(items: list[str], reverse: bool = False) -> str:\n    return \", \".join(reversed(items) if reverse else items)\n",
                ],
            }],
        },
        "refactoring" => SourceExpectation {
            files: &[FileExpectation {
                relative: "src/labels.py",
                accepted: &[
                    "def _join_labels(*parts: str) -> str:\n    return \":\".join(parts)\n\ndef label_pair(left: str, right: str) -> str:\n    return _join_labels(left, right)\n\ndef label_triplet(first: str, second: str, third: str) -> str:\n    return _join_labels(first, second, third)\n",
                    "def _join_labels(*parts: str) -> str:\n    return \":\".join(f\"{part}\" for part in parts)\n\ndef label_pair(left: str, right: str) -> str:\n    return _join_labels(left, right)\n\ndef label_triplet(first: str, second: str, third: str) -> str:\n    return _join_labels(first, second, third)\n",
                ],
            }],
        },
        "test-generation" => SourceExpectation {
            files: &[FileExpectation {
                relative: "tests/test_temperature.py",
                accepted: &[
                    "import unittest\n\nfrom src.temperature import classify\n\nclass TemperatureTests(unittest.TestCase):\n    def test_freezing(self):\n        self.assertEqual(classify(-1), \"freezing\")\n\n    def test_hot(self):\n        self.assertEqual(classify(31), \"hot\")\n\n    def test_freezing_boundary(self):\n        for celsius, expected in ((-0.1, \"freezing\"), (0, \"mild\"), (0.1, \"mild\")):\n            with self.subTest(celsius=celsius):\n                self.assertEqual(classify(celsius), expected)\n\n    def test_hot_boundary(self):\n        for celsius, expected in ((29.9, \"mild\"), (30, \"mild\"), (30.1, \"hot\")):\n            with self.subTest(celsius=celsius):\n                self.assertEqual(classify(celsius), expected)\n\nif __name__ == \"__main__\":\n    unittest.main()\n",
                    "import unittest\n\nfrom src.temperature import classify\n\nclass TemperatureTests(unittest.TestCase):\n    def test_freezing(self):\n        self.assertEqual(classify(-1), \"freezing\")\n\n    def test_hot(self):\n        self.assertEqual(classify(31), \"hot\")\n\n    def test_mild_lower_boundary(self):\n        self.assertEqual(classify(0), \"mild\")\n\n    def test_mild_upper_boundary(self):\n        self.assertEqual(classify(30), \"mild\")\n\nif __name__ == \"__main__\":\n    unittest.main()\n",
                    "import math\n\nimport unittest\n\nfrom src.temperature import classify\n\nclass TemperatureTests(unittest.TestCase):\n    def test_freezing(self):\n        self.assertEqual(classify(-1), \"freezing\")\n\n    def test_hot(self):\n        self.assertEqual(classify(31), \"hot\")\n\n    def test_freezing_boundary(self):\n        cases = (\n            (math.nextafter(0.0, -math.inf), \"freezing\"),\n            (0.0, \"mild\"),\n            (math.nextafter(0.0, math.inf), \"mild\"),\n        )\n        for celsius, expected in cases:\n            with self.subTest(celsius=celsius):\n                self.assertEqual(classify(celsius), expected)\n\n    def test_hot_boundary(self):\n        cases = (\n            (math.nextafter(30.0, -math.inf), \"mild\"),\n            (30.0, \"mild\"),\n            (math.nextafter(30.0, math.inf), \"hot\")\n        )\n        for celsius, expected in cases:\n            with self.subTest(celsius=celsius):\n                self.assertEqual(classify(celsius), expected)\n\n\nif __name__ == \"__main__\":\n    unittest.main()\n",
                    "import unittest\n\nfrom src.temperature import classify\n\nclass TemperatureTests(unittest.TestCase):\n    def test_freezing(self):\n        self.assertEqual(classify(-1), \"freezing\")\n\n    def test_hot(self):\n        self.assertEqual(classify(31), \"hot\")\n\n    def test_freezing_boundary(self):\n        for celsius, expected in [\n            (-0.1, \"freezing\"),\n            (0, \"mild\"),\n            (0.1, \"mild\"),\n        ]:\n            with self.subTest(celsius=celsius):\n                self.assertEqual(classify(celsius), expected)\n\n    def test_hot_boundary(self):\n        for celsius, expected in [\n            (29.9, \"mild\"),\n            (30, \"mild\"),\n            (30.1, \"hot\"),\n        ]:\n            with self.subTest(celsius=celsius):\n                self.assertEqual(classify(celsius), expected)\n\n\nif __name__ == \"__main__\":\n    unittest.main()\n",
                    "import unittest\n\nfrom src.temperature import classify\n\nclass TemperatureTests(unittest.TestCase):\n    def test_freezing(self):\n        self.assertEqual(classify(-1), \"freezing\")\n\n    def test_hot(self):\n        self.assertEqual(classify(31), \"hot\")\n\n    def test_freezing_boundary(self):\n        for celsius, expected in (\n            (-0.1, \"freezing\"),\n            (0, \"mild\"),\n            (0.1, \"mild\"),\n        ):\n            with self.subTest(celsius=celsius):\n                self.assertEqual(classify(celsius), expected)\n\n    def test_hot_boundary(self):\n        for celsius, expected in (\n            (29.9, \"mild\"),\n            (30, \"mild\"),\n            (30.1, \"hot\"),\n        ):\n            with self.subTest(celsius=celsius):\n                self.assertEqual(classify(celsius), expected)\n\n\nif __name__ == \"__main__\":\n    unittest.main()\n",
                    "import unittest\n\nfrom src.temperature import classify\n\nclass TemperatureTests(unittest.TestCase):\n    def test_freezing(self):\n        self.assertEqual(classify(-1), \"freezing\")\n\n    def test_hot(self):\n        self.assertEqual(classify(31), \"hot\")\n\n    def test_freezing_boundary(self):\n        for celsius, expected in [(-0.1, \"freezing\"), (0, \"mild\"), (0.1, \"mild\")]:\n            with self.subTest(celsius=celsius):\n                self.assertEqual(classify(celsius), expected)\n\n    def test_hot_boundary(self):\n        for celsius, expected in [(29.9, \"mild\"), (30, \"mild\"), (30.1, \"hot\")]:\n            with self.subTest(celsius=celsius):\n                self.assertEqual(classify(celsius), expected)\n\nif __name__ == \"__main__\":\n    unittest.main()\n",
                ],
            }],
        },
        _ => return None,
    })
}

fn canonical_source(source: &str) -> String {
    source
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.trim().is_empty() && !line.trim_start().starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n")
        .trim_end()
        .to_owned()
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Evaluation {
    Passed,
    Failed(String),
}

fn source_satisfies(path: &Path, expectation: &SourceExpectation) -> Result<Evaluation, String> {
    for file in expectation.files {
        let source_path = path.join(file.relative);
        let metadata = match fs::metadata(&source_path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Evaluation::Failed(format!(
                    "required output file `{}` is missing from the workload workspace; inspect the agent log and workload instructions",
                    file.relative
                )));
            }
            Err(error) => {
                return Err(format!(
                    "cannot inspect output file `{}`: {error}",
                    file.relative
                ));
            }
        };
        if !metadata.is_file() {
            return Ok(Evaluation::Failed(format!(
                "required output `{}` is not a file",
                file.relative
            )));
        }
        if metadata.len() > MAX_PROMPT_BYTES {
            return Ok(Evaluation::Failed(format!(
                "output file `{}` exceeds the {} byte grading limit",
                file.relative, MAX_PROMPT_BYTES
            )));
        }
        let source = fs::read_to_string(&source_path).map_err(|error| {
            format!(
                "cannot read workload implementation {}: {error}",
                file.relative
            )
        })?;
        let actual = canonical_source(&source);
        if !file
            .accepted
            .iter()
            .map(|candidate| canonical_source(candidate))
            .any(|candidate| candidate == actual)
        {
            return Ok(Evaluation::Failed(format!(
                "output file `{}` does not match an accepted solution for this fixed fixture; inspect the workload instructions and file (equivalent solutions may not be recognized)",
                file.relative
            )));
        }
    }
    Ok(Evaluation::Passed)
}

/// Grade a run's materialized workload without treating a zero exit code as
/// task success.  Grading is implemented in Rust over evaluator-owned,
/// immutable expectations; no agent-controlled tests, subprocess, shell, or
/// inherited credentials are executed.
pub(crate) fn evaluate(
    name: &str,
    workspace: &Path,
    agent_output: &str,
) -> Result<Evaluation, String> {
    if name == "repository-navigation" {
        let response = agent_output.to_ascii_lowercase();
        let missing = ["src/cli.py", "src/registry.py", "lookup"]
            .into_iter()
            .filter(|item| !response.contains(item))
            .collect::<Vec<_>>();
        return Ok(if missing.is_empty() {
            Evaluation::Passed
        } else {
            Evaluation::Failed(format!(
                "final response is missing required references: {}",
                missing.join(", ")
            ))
        });
    }
    if let Some(expectation) = source_expectation(name) {
        return source_satisfies(workspace, &expectation);
    }
    let directory = if adapters::find(name).is_some() {
        workspace.join("external-workload").join("asb-trial")
    } else {
        return Err(format!("no grader is available for workload `{name}`"));
    };
    let implementation = directory.join("src/task.py");
    let relative = "external-workload/asb-trial/src/task.py";
    let metadata = match fs::metadata(&implementation) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Evaluation::Failed(format!(
                "required adapter trial output `{relative}` is missing from the workload workspace; inspect the agent log and trial instructions"
            )));
        }
        Err(error) => {
            return Err(format!(
                "cannot inspect adapter trial output `{relative}`: {error}"
            ));
        }
    };
    if !metadata.is_file() {
        return Ok(Evaluation::Failed(format!(
            "adapter trial output `{relative}` is not a file"
        )));
    }
    if metadata.len() > MAX_PROMPT_BYTES {
        return Ok(Evaluation::Failed(format!(
            "adapter trial output `{relative}` exceeds the {MAX_PROMPT_BYTES} byte grading limit"
        )));
    }
    let source = fs::read_to_string(&implementation).map_err(|error| {
        format!(
            "cannot read adapter trial implementation {}: {error}",
            relative
        )
    })?;
    let expected_label = adapter_trial_label(name);
    // Accept equivalent implementations, not only one formatter spelling.
    // The function and label are still required in the evaluator-owned file,
    // so dead markers or unrelated edits cannot satisfy the trial.
    let normalized = canonical_source(&source);
    let has_normalize = normalized.contains("def normalize(value: str) -> str:");
    let has_lowercase = normalized.contains(".lower()");
    let has_hyphenation = normalized.contains("replace(' ', '-')")
        || normalized.contains("replace(\" \", \"-\")")
        || normalized.contains("'-'.join(value.lower().split())")
        || normalized.contains("\"-\".join(value.lower().split())");
    let has_label = normalized.contains(&format!(
        "def suite_label() -> str:\n    return \"{}\"",
        expected_label
    ));
    Ok(
        if has_normalize
            && has_lowercase
            && has_hyphenation
            && has_label
            && !normalized.contains("if False")
        {
            Evaluation::Passed
        } else {
            Evaluation::Failed(format!(
                "adapter-smoke output `{relative}` does not match the accepted trial implementation; check normalize and suite_label (equivalent solutions may not be recognized)"
            ))
        },
    )
}

fn copy_tree(source: &Path, destination: &Path) -> Result<usize, String> {
    fs::create_dir_all(destination).map_err(|error| {
        format!(
            "cannot create external workload directory {}: {error}",
            destination.display()
        )
    })?;
    let mut added = 0;
    for relative in copy_plan(source)? {
        let source_path = source.join(&relative);
        let destination_path = destination.join(&relative);
        let kind = fs::symlink_metadata(&source_path).map_err(|error| {
            format!(
                "cannot inspect external workload entry {}: {error}",
                source_path.display()
            )
        })?;
        if kind.is_dir() {
            fs::create_dir_all(&destination_path).map_err(|error| {
                format!(
                    "cannot create external workload directory {}: {error}",
                    destination_path.display()
                )
            })?;
        } else if kind.is_file() && !destination_path.exists() {
            fs::copy(&source_path, &destination_path).map_err(|error| {
                format!(
                    "cannot copy external workload file {}: {error}",
                    source_path.display()
                )
            })?;
            added += 1;
        }
    }
    Ok(added)
}

/// Return the exact deterministic pre-order traversal used by `copy_tree`.
/// Sorting at every directory level prevents upstream archive extraction order
/// from changing the request body sent to a deterministic workload runner.
fn copy_plan(source: &Path) -> Result<Vec<PathBuf>, String> {
    fn visit(source: &Path, relative: &Path, plan: &mut Vec<PathBuf>) -> Result<(), String> {
        let mut entries = fs::read_dir(source)
            .map_err(|error| {
                format!(
                    "cannot read external workload source {}: {error}",
                    source.display()
                )
            })?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| format!("cannot inspect external workload source: {error}"))?;
        sort_entries(&mut entries);
        for entry in entries {
            let next = relative.join(entry.file_name());
            plan.push(next.clone());
            if entry
                .file_type()
                .map_err(|error| {
                    format!(
                        "cannot inspect external workload entry {}: {error}",
                        entry.path().display()
                    )
                })?
                .is_dir()
            {
                visit(&entry.path(), &next, plan)?;
            }
        }
        Ok(())
    }

    let mut plan = Vec::new();
    visit(source, Path::new(""), &mut plan)?;
    Ok(plan)
}

fn sort_entries(entries: &mut [fs::DirEntry]) {
    sort_by_name(entries, |entry| entry.file_name());
}

fn sort_by_name<T>(entries: &mut [T], name: impl Fn(&T) -> std::ffi::OsString) {
    entries.sort_by_key(name);
}

fn set_executable(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(path)
            .map_err(|error| format!("cannot inspect fixture {}: {error}", path.display()))?
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).map_err(|error| {
            format!("cannot make fixture {} executable: {error}", path.display())
        })?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn set_executable_io(path: &Path) -> Result<(), std::io::Error> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(path)?.permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions)?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// Load one workload prompt by name and report the available file names on a miss.
pub fn load(name: &str, directory: &Path) -> Result<String, String> {
    let id = name.strip_prefix("original.").unwrap_or(name);
    if id.is_empty()
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
    {
        return Err(format!("invalid workload name `{name}`"));
    }
    let path = directory.join(format!("{id}.txt"));
    let metadata = fs::metadata(&path).map_err(|_| missing(name, directory))?;
    if !metadata.is_file() {
        return Err(missing(name, directory));
    }
    if metadata.len() > MAX_PROMPT_BYTES {
        return Err(format!(
            "workload `{name}` exceeds 64 KiB in {}",
            path.display()
        ));
    }
    let prompt = fs::read_to_string(&path).map_err(|error| {
        format!(
            "cannot read workload `{name}` from {}: {error}",
            path.display()
        )
    })?;
    if prompt.trim().is_empty() {
        return Err(format!("workload `{name}` is empty: {}", path.display()));
    }
    Ok(prompt)
}

pub fn load_trial(
    name: &str,
    trial: Option<&str>,
    trial_set: Option<&str>,
    directory: &Path,
) -> Result<String, String> {
    let prompt = load(name, directory)?;
    let Some(trial) = trial else {
        return Ok(prompt);
    };
    let set_name = trial_set.ok_or_else(|| {
        format!(
            "workload `{name}` selected trial `{trial}` without a trial-set TOML; run `asb workload tui`"
        )
    })?;
    if Path::new(set_name).is_absolute()
        || Path::new(set_name)
            .components()
            .any(|component| component == std::path::Component::ParentDir)
    {
        return Err(format!("invalid workload trial-set path `{set_name}`"));
    }
    let config_path = directory.join(set_name);
    let config = read_trial_set(&config_path)?;
    if config.workload != name {
        return Err(format!(
            "trial set {} belongs to `{}`, not `{name}`",
            config_path.display(),
            config.workload
        ));
    }
    let selected = config
        .trials
        .iter()
        .find(|candidate| candidate.id == trial)
        .ok_or_else(|| {
            format!(
                "trial `{trial}` is not enabled in {}",
                config_path.display()
            )
        })?;
    let trial_prompt = directory
        .join(name)
        .join("trials")
        .join(format!("{}.txt", selected.id));
    if trial_prompt.is_file() {
        return load_prompt_file(&trial_prompt, name);
    }
    Ok(format!(
        "{prompt}\n\nSelected external workload trial: {}\n{}\n",
        selected.id, selected.description
    ))
}

pub fn read_trial_set(path: &Path) -> Result<TrialSet, String> {
    let text = fs::read_to_string(path)
        .map_err(|error| format!("cannot read trial set {}: {error}", path.display()))?;
    let config: TrialSet = toml::from_str(&text)
        .map_err(|error| format!("invalid trial set {}: {error}", path.display()))?;
    if config.workload.trim().is_empty() || config.trials.is_empty() {
        return Err(format!(
            "trial set {} must contain a workload and at least one trial",
            path.display()
        ));
    }
    Ok(config)
}

pub fn trial_ids(name: &str) -> Vec<Trial> {
    if let Some(adapter) = adapters::find(name) {
        return adapter.trial_ids();
    }
    let ids: &[(&str, &str)] = match name {
        _ => &[
            ("smoke", "Small local smoke trial."),
            ("standard", "Standard workload trial."),
            ("extended", "Extended workload trial."),
        ],
    };
    ids.iter()
        .map(|(id, description)| Trial {
            id: (*id).into(),
            description: (*description).into(),
        })
        .collect()
}

/// Names backed by first-class external-suite adapters.
#[cfg(test)]
pub(crate) fn external_adapter_names() -> impl Iterator<Item = &'static str> {
    adapters::names()
}

pub fn write_default_trial_set(name: &str, directory: &Path) -> Result<(), std::io::Error> {
    let path = directory.join(format!("{name}.workload.toml"));
    if path.exists() {
        return Ok(());
    }
    let config = TrialSet {
        workload: name.into(),
        trials: trial_ids(name),
    };
    let text = toml::to_string_pretty(&config).map_err(std::io::Error::other)?;
    fs::write(path, text)
}

fn load_prompt_file(path: &Path, name: &str) -> Result<String, String> {
    let metadata = fs::metadata(path).map_err(|error| {
        format!(
            "cannot inspect trial `{name}` at {}: {error}",
            path.display()
        )
    })?;
    if metadata.len() > MAX_PROMPT_BYTES {
        return Err(format!(
            "trial `{name}` exceeds 64 KiB in {}",
            path.display()
        ));
    }
    let prompt = fs::read_to_string(path).map_err(|error| {
        format!(
            "cannot read trial `{name}` from {}: {error}",
            path.display()
        )
    })?;
    if prompt.trim().is_empty() {
        return Err(format!("trial `{name}` is empty: {}", path.display()));
    }
    Ok(prompt)
}

fn missing(name: &str, directory: &Path) -> String {
    let mut available = fs::read_dir(directory)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            (path.extension().and_then(|value| value.to_str()) == Some("txt"))
                .then(|| path.file_stem()?.to_str().map(str::to_owned))
                .flatten()
        })
        .collect::<Vec<_>>();
    available.sort();
    let candidates = available.iter().map(String::as_str).collect::<Vec<_>>();
    let suggestion = suggestions::closest(name, &candidates)
        .map(|candidate| format!("; did you mean `{candidate}`?"))
        .unwrap_or_default();
    let choices = if available.is_empty() {
        "none".to_owned()
    } else {
        available.join(", ")
    };
    format!(
        "workload `{name}` was not found in {}; available: {choices}{suggestion}",
        directory.display()
    )
}

#[cfg(test)]
mod tests {
    use super::{
        copy_plan, copy_tree, load, load_trial, materialize_external_adapter, populate,
        prepare_external, prepare_fixture, write_default_trial_set,
    };
    use std::fs;
    use std::path::PathBuf;

    fn evaluate(name: &str, workspace: &std::path::Path, output: &str) -> Result<bool, String> {
        super::evaluate(name, workspace, output).map(|value| value == super::Evaluation::Passed)
    }

    #[test]
    fn grade_explains_missing_wrong_large_and_unavailable_inputs() {
        use super::Evaluation;
        let root =
            std::env::temp_dir().join(format!("asb-diagnostic-grades-{}", std::process::id()));
        fs::create_dir_all(root.join("src")).unwrap();
        let grade = |name: &str, output: &str| super::evaluate(name, &root, output).unwrap();
        assert_eq!(
            grade("bug-fix", ""),
            Evaluation::Failed("required output file `src/parser.py` is missing from the workload workspace; inspect the agent log and workload instructions".into())
        );
        fs::write(root.join("src/parser.py"), "not the solution").unwrap();
        assert!(
            matches!(grade("bug-fix", ""), Evaluation::Failed(message) if message.contains("does not match an accepted solution") && !message.contains("not the solution"))
        );
        fs::write(
            root.join("src/parser.py"),
            vec![b'x'; super::MAX_PROMPT_BYTES as usize + 1],
        )
        .unwrap();
        assert!(
            matches!(grade("bug-fix", ""), Evaluation::Failed(message) if message.contains("exceeds") && message.contains("src/parser.py"))
        );
        assert!(
            matches!(grade("repository-navigation", "src/cli.py"), Evaluation::Failed(message) if message.contains("src/registry.py") && message.contains("lookup") && !message.contains("src/cli.py"))
        );
        assert!(
            super::evaluate("unknown", &root, "")
                .unwrap_err()
                .contains("no grader is available")
        );
        assert!(
            matches!(grade("agentbench", ""), Evaluation::Failed(message) if message.contains("adapter trial output") && message.contains("missing"))
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn loads_original_alias_and_rejects_path_traversal() {
        let directory = std::env::temp_dir().join(format!("asb-workloads-{}", std::process::id()));
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join("demo.txt"), "demo prompt").unwrap();
        assert_eq!(load("original.demo", &directory).unwrap(), "demo prompt");
        assert!(load("../secret", &directory).is_err());
        let _ = fs::remove_file(directory.join("demo.txt"));
        let _ = fs::remove_dir(directory);
    }

    #[test]
    fn populate_copies_bundled_prompts_without_overwriting_custom_files() {
        let directory =
            std::env::temp_dir().join(format!("asb-bundled-workloads-{}", std::process::id()));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join("bug-fix.txt"), "custom").unwrap();
        let added = populate(&directory).expect("populate succeeds");
        assert_eq!(added, 6);
        assert_eq!(
            fs::read_to_string(directory.join("bug-fix.txt")).unwrap(),
            "custom"
        );
        assert!(
            !fs::read_to_string(directory.join("repository-navigation.txt"))
                .unwrap()
                .is_empty()
        );
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn every_builtin_fixture_materializes_supporting_files() {
        let directory = std::env::temp_dir().join(format!("asb-fixtures-{}", std::process::id()));
        let _ = fs::remove_dir_all(&directory);
        for name in [
            "bug-fix",
            "build-repair",
            "dependency-migration",
            "feature-addition",
            "refactoring",
            "repository-navigation",
            "test-generation",
        ] {
            let fixture_dir = directory.join(name);
            assert!(prepare_fixture(name, &fixture_dir).unwrap() > 0, "{name}");
            assert!(fixture_dir.join("README.md").is_file(), "{name} README");
            assert!(fixture_dir.join("tests").is_dir(), "{name} tests");
        }
        assert!(directory.join("build-repair/build.sh").is_file());
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn installed_external_source_is_copied_without_overwriting_workspace_edits() {
        let root = std::env::temp_dir().join(format!("asb-external-copy-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let source = root.join("workloads/swe-bench/source");
        let workspace = root.join("workspace");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("README.md"), "upstream").unwrap();
        fs::create_dir_all(workspace.join("external-workload")).unwrap();
        fs::write(workspace.join("external-workload/README.md"), "edited").unwrap();
        assert_eq!(
            prepare_external("swe-bench", &root.join("workloads"), &workspace).unwrap(),
            0
        );
        assert_eq!(
            fs::read_to_string(workspace.join("external-workload/README.md")).unwrap(),
            "edited"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn external_source_copy_uses_deterministic_lexical_traversal() {
        let root =
            std::env::temp_dir().join(format!("asb-external-copy-order-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let source = root.join("source");
        let destination = root.join("destination");
        fs::create_dir_all(source.join("z-last/inner-z")).unwrap();
        fs::create_dir_all(source.join("a-first/inner-z")).unwrap();
        fs::write(source.join("z-last/file"), "z").unwrap();
        fs::write(source.join("a-first/file"), "a").unwrap();
        fs::write(source.join("z-last/inner-z/file"), "z-inner").unwrap();
        fs::write(source.join("a-first/inner-z/file"), "a-inner").unwrap();

        // The explicit unsorted input proves the negative case without
        // depending on unspecified filesystem directory iteration order.
        let mut unsorted = vec![PathBuf::from("z-last"), PathBuf::from("a-first")];
        super::sort_by_name(&mut unsorted, |path| path.clone().into_os_string());
        assert_eq!(
            unsorted,
            vec![PathBuf::from("a-first"), PathBuf::from("z-last")]
        );
        assert_eq!(
            copy_plan(&source).unwrap(),
            vec![
                PathBuf::from("a-first"),
                PathBuf::from("a-first/file"),
                PathBuf::from("a-first/inner-z"),
                PathBuf::from("a-first/inner-z/file"),
                PathBuf::from("z-last"),
                PathBuf::from("z-last/file"),
                PathBuf::from("z-last/inner-z"),
                PathBuf::from("z-last/inner-z/file"),
            ]
        );
        copy_tree(&source, &destination).unwrap();
        assert_eq!(
            fs::read_to_string(destination.join("a-first/inner-z/file")).unwrap(),
            "a-inner"
        );
        assert_eq!(
            fs::read_to_string(destination.join("z-last/inner-z/file")).unwrap(),
            "z-inner"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn external_adapter_trial_is_materialized_and_graded() {
        let root = std::env::temp_dir().join(format!("asb-external-trial-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let workloads = root.join("workloads");
        let workspace = root.join("workspace");
        fs::create_dir_all(&workloads).unwrap();
        materialize_external_adapter("agentbench", &workloads).unwrap();
        prepare_external("agentbench", &workloads, &workspace).unwrap();
        let task = workspace.join("external-workload/asb-trial/src/task.py");
        assert!(!evaluate("agentbench", &workspace, "").unwrap());
        fs::write(
            &task,
            "def normalize(value: str) -> str:\n    return value.strip().lower().replace(' ', '-')\n\ndef suite_label() -> str:\n    return \"agentbench\"\n",
        )
        .unwrap();
        // The grader does not execute or read agent-controlled tests.
        fs::write(
            workspace.join("external-workload/asb-trial/tests/test_task.py"),
            "# intentionally tampered by the agent\n",
        )
        .unwrap();
        assert!(evaluate("agentbench", &workspace, "").unwrap());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn bundled_grader_rejects_the_unfixed_fixture() {
        let root = std::env::temp_dir().join(format!("asb-builtin-grade-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let workspace = root.join("workspace");
        prepare_fixture("bug-fix", &workspace).unwrap();
        assert!(!evaluate("bug-fix", &workspace, "").unwrap());
        fs::write(
            workspace.join("src/parser.py"),
            "def parse_path(value: str) -> list[str]:\n    return [segment for segment in value.split('/') if segment]\n",
        )
        .unwrap();
        assert!(evaluate("bug-fix", &workspace, "").unwrap());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn bundled_grader_rejects_dead_or_unused_markers() {
        let root =
            std::env::temp_dir().join(format!("asb-builtin-semantic-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let workspace = root.join("workspace");
        prepare_fixture("feature-addition", &workspace).unwrap();
        fs::write(
            workspace.join("src/formatter.py"),
            "def format_items(items: list[str]) -> str:\n    reverse: bool = False\n    if False:\n        return \", \".join(reversed(items))\n    return \", \".join(items)\n",
        )
        .unwrap();
        assert!(!evaluate("feature-addition", &workspace, "").unwrap());
        fs::write(
            workspace.join("src/formatter.py"),
            "def format_items(items: list[str], reverse: bool = False) -> str:\nordered = list(reversed(items)) if reverse else items\n    return \", \".join(ordered)\n",
        )
        .unwrap();
        assert!(!evaluate("feature-addition", &workspace, "").unwrap());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn bundled_grader_accepts_the_equivalent_inline_feature_solution() {
        let root = std::env::temp_dir().join(format!("asb-feature-variant-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let workspace = root.join("workspace");
        prepare_fixture("feature-addition", &workspace).unwrap();
        fs::write(
            workspace.join("src/formatter.py"),
            "def format_items(items: list[str], reverse: bool = False) -> str:\n    return \", \".join(reversed(items) if reverse else items)\n",
        )
        .unwrap();
        assert!(evaluate("feature-addition", &workspace, "").unwrap());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn bundled_grader_accepts_valid_refactoring_and_test_generation_variants() {
        let root =
            std::env::temp_dir().join(format!("asb-builtin-variants-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);

        let refactoring_workspace = root.join("refactoring");
        prepare_fixture("refactoring", &refactoring_workspace).unwrap();
        fs::write(
            refactoring_workspace.join("src/labels.py"),
            "def _join_labels(*parts: str) -> str:\n    return \":\".join(f\"{part}\" for part in parts)\n\ndef label_pair(left: str, right: str) -> str:\n    return _join_labels(left, right)\n\ndef label_triplet(first: str, second: str, third: str) -> str:\n    return _join_labels(first, second, third)\n",
        )
        .unwrap();
        assert!(evaluate("refactoring", &refactoring_workspace, "").unwrap());

        let test_generation_workspace = root.join("test-generation");
        prepare_fixture("test-generation", &test_generation_workspace).unwrap();
        fs::write(
            test_generation_workspace.join("tests/test_temperature.py"),
            "import unittest\n\nfrom src.temperature import classify\n\nclass TemperatureTests(unittest.TestCase):\n    def test_freezing(self):\n        self.assertEqual(classify(-1), \"freezing\")\n\n    def test_hot(self):\n        self.assertEqual(classify(31), \"hot\")\n\n    def test_freezing_boundary(self):\n        for celsius, expected in ((-0.1, \"freezing\"), (0, \"mild\"), (0.1, \"mild\")):\n            with self.subTest(celsius=celsius):\n                self.assertEqual(classify(celsius), expected)\n\n    def test_hot_boundary(self):\n        for celsius, expected in ((29.9, \"mild\"), (30, \"mild\"), (30.1, \"hot\")):\n            with self.subTest(celsius=celsius):\n                self.assertEqual(classify(celsius), expected)\n\nif __name__ == \"__main__\":\n    unittest.main()\n",
        )
        .unwrap();
        assert!(evaluate("test-generation", &test_generation_workspace, "").unwrap());
        fs::write(
            test_generation_workspace.join("tests/test_temperature.py"),
            "import unittest\n\nfrom src.temperature import classify\n\nclass TemperatureTests(unittest.TestCase):\n    def test_freezing(self):\n        self.assertEqual(classify(-1), \"freezing\")\n\n    def test_hot(self):\n        self.assertEqual(classify(31), \"hot\")\n\n    def test_freezing_boundary(self):\n        for celsius, expected in [\n            (-0.1, \"freezing\"),\n            (0, \"mild\"),\n            (0.1, \"mild\"),\n        ]:\n            with self.subTest(celsius=celsius):\n                self.assertEqual(classify(celsius), expected)\n\n    def test_hot_boundary(self):\n        for celsius, expected in [\n            (29.9, \"mild\"),\n            (30, \"mild\"),\n            (30.1, \"hot\"),\n        ]:\n            with self.subTest(celsius=celsius):\n                self.assertEqual(classify(celsius), expected)\n\n\nif __name__ == \"__main__\":\n    unittest.main()\n",
        )
        .unwrap();
        assert!(evaluate("test-generation", &test_generation_workspace, "").unwrap());
        fs::write(
            test_generation_workspace.join("tests/test_temperature.py"),
            "import math\n\nimport unittest\n\nfrom src.temperature import classify\n\nclass TemperatureTests(unittest.TestCase):\n    def test_freezing(self):\n        self.assertEqual(classify(-1), \"freezing\")\n\n    def test_hot(self):\n        self.assertEqual(classify(31), \"hot\")\n\n    def test_freezing_boundary(self):\n        cases = (\n            (math.nextafter(0.0, -math.inf), \"freezing\"),\n            (0.0, \"mild\"),\n            (math.nextafter(0.0, math.inf), \"mild\"),\n        )\n        for celsius, expected in cases:\n            with self.subTest(celsius=celsius):\n                self.assertEqual(classify(celsius), expected)\n\n    def test_hot_boundary(self):\n        cases = (\n            (math.nextafter(30.0, -math.inf), \"mild\"),\n            (30.0, \"mild\"),\n            (math.nextafter(30.0, math.inf), \"hot\")\n        )\n        for celsius, expected in cases:\n            with self.subTest(celsius=celsius):\n                self.assertEqual(classify(celsius), expected)\n\n\nif __name__ == \"__main__\":\n    unittest.main()\n",
        )
        .unwrap();
        assert!(evaluate("test-generation", &test_generation_workspace, "").unwrap());
        fs::write(
            test_generation_workspace.join("tests/test_temperature.py"),
            "import unittest\n\nfrom src.temperature import classify\n\nclass TemperatureTests(unittest.TestCase):\n    def test_freezing(self):\n        self.assertEqual(classify(-1), \"freezing\")\n\n    def test_hot(self):\n        self.assertEqual(classify(31), \"hot\")\n\n    def test_mild_lower_boundary(self):\n        self.assertEqual(classify(0), \"mild\")\n\n    def test_mild_upper_boundary(self):\n        self.assertEqual(classify(30), \"mild\")\n\nif __name__ == \"__main__\":\n    unittest.main()\n",
        )
        .unwrap();
        assert!(evaluate("test-generation", &test_generation_workspace, "").unwrap());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn bundled_grader_accepts_multiline_tuple_test_generation_variant() {
        let root = std::env::temp_dir().join(format!(
            "asb-test-generation-multiline-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        let workspace = root.join("workspace");
        prepare_fixture("test-generation", &workspace).unwrap();
        fs::write(
            workspace.join("tests/test_temperature.py"),
            "import unittest\n\nfrom src.temperature import classify\n\nclass TemperatureTests(unittest.TestCase):\n    def test_freezing(self):\n        self.assertEqual(classify(-1), \"freezing\")\n\n    def test_hot(self):\n        self.assertEqual(classify(31), \"hot\")\n\n    def test_freezing_boundary(self):\n        for celsius, expected in (\n            (-0.1, \"freezing\"),\n            (0, \"mild\"),\n            (0.1, \"mild\"),\n        ):\n            with self.subTest(celsius=celsius):\n                self.assertEqual(classify(celsius), expected)\n\n    def test_hot_boundary(self):\n        for celsius, expected in (\n            (29.9, \"mild\"),\n            (30, \"mild\"),\n            (30.1, \"hot\"),\n        ):\n            with self.subTest(celsius=celsius):\n                self.assertEqual(classify(celsius), expected)\n\n\nif __name__ == \"__main__\":\n    unittest.main()\n",
        )
        .unwrap();
        assert!(evaluate("test-generation", &workspace, "").unwrap());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn bundled_grader_accepts_single_line_list_test_generation_variant() {
        let root = std::env::temp_dir().join(format!(
            "asb-test-generation-list-single-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        let workspace = root.join("workspace");
        prepare_fixture("test-generation", &workspace).unwrap();
        fs::write(
            workspace.join("tests/test_temperature.py"),
            "import unittest\n\nfrom src.temperature import classify\n\nclass TemperatureTests(unittest.TestCase):\n    def test_freezing(self):\n        self.assertEqual(classify(-1), \"freezing\")\n\n    def test_hot(self):\n        self.assertEqual(classify(31), \"hot\")\n\n    def test_freezing_boundary(self):\n        for celsius, expected in [(-0.1, \"freezing\"), (0, \"mild\"), (0.1, \"mild\")]:\n            with self.subTest(celsius=celsius):\n                self.assertEqual(classify(celsius), expected)\n\n    def test_hot_boundary(self):\n        for celsius, expected in [(29.9, \"mild\"), (30, \"mild\"), (30.1, \"hot\")]:\n            with self.subTest(celsius=celsius):\n                self.assertEqual(classify(celsius), expected)\n\nif __name__ == \"__main__\":\n    unittest.main()\n",
        )
        .unwrap();
        assert!(evaluate("test-generation", &workspace, "").unwrap());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn repository_navigation_grades_only_the_explicit_final_response() {
        let root =
            std::env::temp_dir().join(format!("asb-navigation-grade-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let workspace = root.join("workspace");
        prepare_fixture("repository-navigation", &workspace).unwrap();
        let tool_trace = "unrelated final answer";
        assert!(!evaluate("repository-navigation", &workspace, tool_trace).unwrap());
        let answer = "The call path goes from src/cli.py to src/registry.py through lookup.";
        assert!(evaluate("repository-navigation", &workspace, answer).unwrap());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn external_grader_rejects_unreachable_adapter_code() {
        let root =
            std::env::temp_dir().join(format!("asb-external-semantic-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let workloads = root.join("workloads");
        let workspace = root.join("workspace");
        fs::create_dir_all(&workloads).unwrap();
        materialize_external_adapter("agentbench", &workloads).unwrap();
        prepare_external("agentbench", &workloads, &workspace).unwrap();
        let task = workspace.join("external-workload/asb-trial/src/task.py");
        fs::write(
            &task,
            "def normalize(value: str) -> str:\n    if False:\n        return value.strip().lower().replace(' ', '-')\n    return value\n\ndef suite_label() -> str:\n    return \"agentbench\"\n",
        )
        .unwrap();
        assert!(!evaluate("agentbench", &workspace, "").unwrap());
        fs::write(
            &task,
            "def normalize(value: str) -> str:\n    return value.strip().lower().replace('     ', '-')\n\ndef suite_label() -> str:\n    return \"agentbench\"\n",
        )
        .unwrap();
        assert!(!evaluate("agentbench", &workspace, "").unwrap());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn missing_workload_suggests_a_close_local_name() {
        let directory =
            std::env::temp_dir().join(format!("asb-workload-suggestion-{}", std::process::id()));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join("bug-fix.txt"), "prompt").unwrap();
        let error = load("bug-fx", &directory).expect_err("typo rejected");
        assert!(error.contains("did you mean `bug-fix`?"));
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn trial_sets_round_trip_and_select_a_configured_prompt() {
        let directory = std::env::temp_dir().join(format!("asb-trials-{}", std::process::id()));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir_all(directory.join("terminal-bench/trials")).unwrap();
        fs::write(directory.join("terminal-bench.txt"), "base prompt").unwrap();
        write_default_trial_set("terminal-bench", &directory).unwrap();
        fs::write(
            directory.join("terminal-bench/trials/debug.txt"),
            "debug task",
        )
        .unwrap();
        let prompt = load_trial(
            "terminal-bench",
            Some("debug"),
            Some("terminal-bench.workload.toml"),
            &directory,
        )
        .unwrap();
        assert_eq!(prompt, "debug task");
        assert!(
            load_trial(
                "terminal-bench",
                Some("unknown"),
                Some("terminal-bench.workload.toml"),
                &directory,
            )
            .is_err()
        );
        let _ = fs::remove_dir_all(directory);
    }
}
