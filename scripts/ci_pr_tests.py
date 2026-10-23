#!/usr/bin/env python3
"""Run the bounded, changed-component Rust test gate for pull requests.

The complete serial suite remains the post-merge gate.  This helper keeps the
required pull-request gate short while still exercising the owning component of
every Rust change.  It deliberately uses paths from Git rather than trusting a
caller-supplied test name.
"""

from __future__ import annotations

import argparse
from pathlib import PurePosixPath
import subprocess
import sys


SOURCE_ROOT = PurePosixPath("crates/asb-cli/src")
TEST_ROOT = PurePosixPath("crates/asb-cli/tests")
COMMON_TARGETS = ("config::", "parser::", "project::")


def component_targets(paths: list[str]) -> tuple[str, ...]:
    """Return stable Cargo filters for the changed files.

    A direct integration-test edit selects that test target.  A library source
    edit selects its top-level owning module.  Shared roots select a compact
    cross-command smoke group; the full post-merge suite remains responsible
    for whole-program interactions.
    """
    targets: set[str] = set()
    shared = False
    for value in paths:
        path = PurePosixPath(value)
        if path.is_relative_to(TEST_ROOT):
            relative = path.relative_to(TEST_ROOT)
            if len(relative.parts) == 1 and relative.suffix == ".rs":
                targets.add(f"test:{relative.stem}")
            else:
                shared = True
            continue
        if path.is_relative_to(SOURCE_ROOT):
            relative = path.relative_to(SOURCE_ROOT)
            if len(relative.parts) == 1:
                stem = relative.stem
                if stem in {"lib", "main"}:
                    shared = True
                else:
                    targets.add(f"lib:{stem}::")
            elif relative.parts[0] in {"agents", "collectors", "compare", "install", "providers", "runner", "sweep", "tui", "workloads"}:
                targets.add(f"lib:{relative.parts[0]}::")
            else:
                shared = True
    if shared:
        targets.update(f"lib:{target}" for target in COMMON_TARGETS)
    return tuple(sorted(targets))


def changed_paths(base: str, head: str) -> list[str]:
    result = subprocess.run(
        ["git", "diff", "--name-only", f"{base}...{head}"],
        check=True,
        stdout=subprocess.PIPE,
        text=True,
    )
    return [line for line in result.stdout.splitlines() if line]


def run_target(target: str) -> None:
    kind, name = target.split(":", 1)
    command = ["cargo", "test", "--locked"]
    if kind == "test":
        command.extend(["--test", name])
    else:
        command.extend(["--lib", name])
    command.extend(["--", "--test-threads=1"])
    print("+", " ".join(command), flush=True)
    subprocess.run(command, check=True)


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--base", required=True, help="trusted base commit SHA")
    parser.add_argument("--head", required=True, help="trusted pull-request head SHA")
    args = parser.parse_args(argv)

    paths = changed_paths(args.base, args.head)
    targets = component_targets(paths)
    if not targets:
        print("No Rust test-owning files changed; build and Clippy remain required.")
        return 0
    for target in targets:
        run_target(target)
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
