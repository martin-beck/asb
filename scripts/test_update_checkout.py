#!/usr/bin/env python3
"""Exercise make update against disposable local Git repositories."""

import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parent.parent


def run(directory, *args, check=True):
    env = os.environ.copy()
    env.update({
        "GIT_AUTHOR_NAME": "ASB test", "GIT_AUTHOR_EMAIL": "asb-test@example.invalid",
        "GIT_COMMITTER_NAME": "ASB test", "GIT_COMMITTER_EMAIL": "asb-test@example.invalid",
        "GIT_CONFIG_NOSYSTEM": "1", "GIT_CONFIG_GLOBAL": os.devnull,
        "LC_ALL": "C", "GIT_TERMINAL_PROMPT": "0",
    })
    return subprocess.run(args, cwd=directory, env=env, check=check,
                          stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)


class UpdateTests(unittest.TestCase):
    def setUp(self):
        (ROOT / "target").mkdir(exist_ok=True)
        self.temp = tempfile.TemporaryDirectory(prefix="update-test-", dir=ROOT / "target")
        self.addCleanup(self.temp.cleanup)
        root = Path(self.temp.name)
        self.remote, self.seed, self.client = root / "origin.git", root / "seed", root / "client"
        run(root, "git", "init", "--bare", str(self.remote))
        self.seed.mkdir()
        run(self.seed, "git", "init", "-b", "main")
        (self.seed / "docs").mkdir()
        (self.seed / "scripts").mkdir()
        shutil.copy(ROOT / "Makefile", self.seed / "Makefile")
        shutil.copy(ROOT / "scripts/update_checkout.sh", self.seed / "scripts/update_checkout.sh")
        (self.seed / "docs/asb-dataflow.dot").write_text("old diagram\n")
        (self.seed / "source.txt").write_text("old source\n")
        run(self.seed, "git", "add", ".")
        run(self.seed, "git", "commit", "-m", "initial")
        run(self.seed, "git", "remote", "add", "origin", str(self.remote))
        run(self.seed, "git", "push", "-u", "origin", "main")
        run(root, "git", "clone", "-b", "main", str(self.remote), str(self.client))

    def advance(self, path, content):
        (self.seed / path).write_text(content)
        run(self.seed, "git", "add", path)
        run(self.seed, "git", "commit", "-m", "upstream")
        run(self.seed, "git", "push", "origin", "main")

    def test_modified_and_staged_diagrams_are_backed_up(self):
        path = "docs/asb-dataflow.dot"
        (self.client / path).write_text("staged edit\n")
        run(self.client, "git", "add", path)
        (self.client / path).write_text("working edit\n")
        self.advance(path, "upstream diagram\n")
        result = run(self.client, "make", "update")
        self.assertIn("git stash list", result.stdout)
        self.assertEqual((self.client / path).read_text(), "upstream diagram\n")
        self.assertEqual(run(self.client, "git", "show", f"stash@{{0}}:{path}").stdout, "working edit\n")
        self.assertEqual(run(self.client, "git", "show", f"stash@{{0}}^2:{path}").stdout, "staged edit\n")

    def test_untracked_generated_file_is_preserved_before_upstream_adds_it(self):
        path = "docs/asb-control-flow.html"
        (self.client / path).write_text("local output\n")
        self.advance(path, "upstream output\n")
        run(self.client, "make", "update")
        self.assertEqual((self.client / path).read_text(), "upstream output\n")
        self.assertEqual(run(self.client, "git", "show", f"stash@{{0}}^3:{path}").stdout, "local output\n")

    def test_unrelated_conflicting_change_is_not_stashed_or_overwritten(self):
        (self.client / "docs/asb-dataflow.dot").write_text("local diagram\n")
        (self.client / "source.txt").write_text("local source\n")
        self.advance("source.txt", "upstream source\n")
        result = run(self.client, "make", "update", check=False)
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertEqual((self.client / "source.txt").read_text(), "local source\n")
        self.assertEqual(run(self.client, "git", "show", "stash@{0}:docs/asb-dataflow.dot").stdout, "local diagram\n")

    def test_unrelated_staged_change_remains_staged(self):
        (self.client / "docs/asb-dataflow.dot").write_text("local diagram\n")
        (self.client / "source.txt").write_text("staged source\n")
        run(self.client, "git", "add", "source.txt")
        self.advance("docs/asb-dataflow.dot", "upstream diagram\n")
        run(self.client, "make", "update")
        self.assertEqual((self.client / "source.txt").read_text(), "staged source\n")
        self.assertEqual(run(self.client, "git", "show", ":source.txt").stdout, "staged source\n")

    def test_clean_update_does_not_create_stash(self):
        self.advance("source.txt", "upstream source\n")
        run(self.client, "make", "update")
        self.assertEqual((self.client / "source.txt").read_text(), "upstream source\n")
        self.assertEqual(run(self.client, "git", "stash", "list").stdout, "")

    def test_update_backup_does_not_require_git_identity(self):
        (self.client / "docs/asb-dataflow.dot").write_text("local diagram\n")
        self.advance("docs/asb-dataflow.dot", "upstream diagram\n")
        env = os.environ.copy()
        for name in ("GIT_AUTHOR_NAME", "GIT_AUTHOR_EMAIL", "GIT_COMMITTER_NAME", "GIT_COMMITTER_EMAIL"):
            env.pop(name, None)
        env.update({"GIT_CONFIG_NOSYSTEM": "1", "GIT_CONFIG_GLOBAL": os.devnull})
        result = subprocess.run(["make", "update"], cwd=self.client, env=env,
                                stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertEqual((self.client / "docs/asb-dataflow.dot").read_text(), "upstream diagram\n")

    def test_build_keeps_diagram_generation_explicit(self):
        output = run(ROOT, "make", "-n", "build", "AUTO_UML=/bin/true").stdout
        self.assertIn("cargo build --locked", output)
        self.assertIn("cargo doc --locked", output)
        self.assertIn("--__generate-completion bash", output)
        for name in ("asb-type-diagram.dot", "asb-type-diagram.svg", "asb-type-diagram.mmd",
                     "asb-type-diagram.html", "asb-dataflow.dot", "asb-dataflow.svg",
                     "asb-dataflow.html", "asb-control-flow.dot", "asb-control-flow.svg",
                     "asb-control-flow.html"):
            self.assertNotIn(f"target/{name}", output)
            self.assertNotIn(f"docs/{name}", output)


if __name__ == "__main__":
    unittest.main()
