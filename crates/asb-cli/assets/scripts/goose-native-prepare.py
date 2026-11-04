"""Prepare a pinned Goose source tree for ASB's experimental native hook.

This is an adapter build step, not an ASB user command.  Runtime and replay
logic remains in goose-native-hook.rs; the versioned patch only forwards the
native dispatch future to that module.
"""
import argparse
import hashlib
import os
from pathlib import Path
import subprocess
import sys
import tomllib

SUPPORTED_VERSION = '1.54.0'
SOURCE_HASHES = {
    'crates/goose-cli/Cargo.toml': '8e0fc9794db8f908507b1bfc3931322e5057e846d23fef773276b487b863ed45',
    'crates/goose-cli/src/main.rs': '301d1ff4503b12c3e6cc79a3a8c0a4e0e2b195c238f4f50e88cb00520055a284',
    'crates/goose/Cargo.toml': 'b1c97519c93e79faebb3892ad9985e5a75b16cdf3de9838a06127c78de1449a6',
    'crates/goose/src/agents/extension_manager/lease.rs': 'b2061f486a8baa9b927458a9090900c986fbbe8f03e3835021f00d75cca6fdee',
    'crates/goose/src/agents/extension_manager/mod.rs': '3ac2ca7454c9cca5a5f477f38e1f12fb70e4f34da8b3dfb6b16c0062417cb0a1',
}
PATCHED_HASHES = {
    'crates/goose-cli/Cargo.toml': '85c8c0f90c785454432922210c0f1119e4b635ec380c96013bbdf47d26db28e2',
    'crates/goose-cli/src/main.rs': 'd1b2eb1aebaf8ce161b6ab5c867c4862daf1a05ad0e06f9b61f7ff830eb6db55',
    'crates/goose/Cargo.toml': '84ba15ff6478d61785e3d35db55af2eccc0d7736346a1fc0ad5b7ac7c797fb03',
    'crates/goose/src/agents/extension_manager/lease.rs': '53b847435e061f484f319acfe986aede6e84908cd2751bcee915183bd6a46247',
    'crates/goose/src/agents/extension_manager/mod.rs': 'a266dc22d432df3301b668ea20bcd35e0eebaafd2eb4cdfdfe19f8db81fe97b7',
    'crates/goose/src/agents/extension_manager/asb_native_probe.rs': '38db71d17a667a7412a6c1f4f6458174dc8b9c94d2c3276bcdfd2893f685e407',
}


def matches(source, expected):
    try:
        return bool(expected) and all(hashlib.sha256((source / relative).read_bytes()).hexdigest() == digest
                                      for relative, digest in expected.items())
    except OSError:
        return False


def prepare(source, patch):
    source, patch = Path(source).resolve(), Path(patch).resolve()
    try:
        document = tomllib.loads((source / 'Cargo.toml').read_text(encoding='utf-8'))
        version = document['workspace']['package']['version']
    except (OSError, KeyError, TypeError, tomllib.TOMLDecodeError) as error:
        raise ValueError('Goose source is missing a readable workspace version') from error
    if version != SUPPORTED_VERSION:
        raise ValueError(f'Goose {version!r} is not supported by this hook; expected {SUPPORTED_VERSION}. Update and test the adapter patch for this version.')
    if not patch.is_file():
        raise ValueError('ASB Goose dispatch patch is missing; reinstall ASB')
    if matches(source, PATCHED_HASHES):
        return
    if SOURCE_HASHES and not matches(source, SOURCE_HASHES):
        raise ValueError('Goose dispatch source does not match the supported layout; use a clean source checkout and update the adapter for changed Goose layouts.')
    # A project-local extracted source tree can be nested below ASB's own Git
    # checkout. Without a ceiling Git silently ignores every patch path outside
    # that nested directory while still reporting success.
    environment = os.environ.copy()
    for key in ('GIT_DIR', 'GIT_WORK_TREE', 'GIT_INDEX_FILE'):
        environment.pop(key, None)
    environment['GIT_CEILING_DIRECTORIES'] = str(source.parent)
    command = ['git', 'apply', '--unidiff-zero', '--check', str(patch)]
    try:
        checked = subprocess.run(command, cwd=source, env=environment, stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=False)
    except OSError as error:
        raise ValueError('Cannot run git to prepare Goose; install git and retry') from error
    if checked.returncode:
        raise ValueError('Goose dispatch source does not match the supported layout or is already patched; use a clean source checkout and update the adapter for changed Goose layouts.')
    applied = subprocess.run(['git', 'apply', '--unidiff-zero', str(patch)], cwd=source, env=environment, stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=False)
    if applied.returncode:
        raise ValueError('Goose dispatch patch could not be applied; preserve the source checkout and retry with a clean source tree.')
    if SOURCE_HASHES and not matches(source, PATCHED_HASHES):
        raise ValueError('Goose dispatch patch produced an unexpected source layout; preserve the source checkout for diagnosis.')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('source', type=Path)
    parser.add_argument('--patch', type=Path, default=Path(__file__).with_name('goose-native-dispatch.patch'))
    args = parser.parse_args()
    try:
        prepare(args.source, args.patch)
    except ValueError as error:
        print(f'ASB Goose hook preparation failed: {error}', file=sys.stderr)
        return 1
    print(f'Goose {SUPPORTED_VERSION} source prepared for ASB native-hook compilation.')
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
