//! Grading and fixed expectations owned by the bundled prompt adapter.
use crate::workloads::{Evaluation, MAX_PROMPT_BYTES, canonical_source};
use std::{fs, path::Path};

pub(crate) struct SourceExpectation {
    pub(crate) files: &'static [FileExpectation],
}

pub(crate) struct FileExpectation {
    pub(crate) relative: &'static str,
    /// Complete evaluator-owned solutions accepted for this fixed fixture.
    ///
    /// The grader deliberately compares the whole bounded artifact instead of
    /// looking for a few lexical markers.  That prevents dead, unreachable,
    /// or unused code from being mistaken for a completed task.
    pub(crate) accepted: &'static [&'static str],
}

pub(crate) fn source_expectation(name: &str) -> Option<SourceExpectation> {
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

pub(crate) fn evaluate_builtin(
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
    Err(format!(
        "no bundled grader is available for workload `{name}`"
    ))
}
