"""Build ASB's Goose adapter from a previously downloaded pinned source tree.

This internal installer component keeps Goose's default features and atomically
publishes the resulting project-local executable after a capability probe.
"""
import argparse
import importlib.util
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile

HERE = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location('asb_goose_prepare', HERE / 'goose-native-prepare.py')
prepare = importlib.util.module_from_spec(spec)
spec.loader.exec_module(prepare)
symbol_spec = importlib.util.spec_from_file_location('asb_goose_symbols', HERE / 'goose-profile-symbols.py')
symbols = importlib.util.module_from_spec(symbol_spec)
symbol_spec.loader.exec_module(symbols)


def build(source, target, output, symbol_root, *, offline=False):
    source, target, output = (Path(path).resolve() for path in (source, target, output))
    prepare.prepare(source, HERE / 'goose-native-dispatch.patch')
    environment = os.environ.copy()
    # The pinned source chooses its required Rust toolchain. An inherited
    # override from the ASB build must not silently change that selection.
    environment.pop('RUSTUP_TOOLCHAIN', None)
    environment.pop('ASB_GOOSE_NATIVE_CAPABILITY', None)
    # Keep release optimization while retaining names and line tables for
    # agent-scoped perf profiles. The previously stripped executable made
    # flamegraphs unable to identify Goose's own code.
    environment.update(ASB_GOOSE_HOOK_SOURCE=str(HERE / 'goose-native-hook.rs'),
                       CARGO_TARGET_DIR=str(target), CARGO_PROFILE_RELEASE_STRIP='none',
                       CARGO_PROFILE_RELEASE_DEBUG='1', CARGO_PROFILE_RELEASE_SPLIT_DEBUGINFO='off')
    args = ['cargo', 'build', '--locked', '--release', '-p', 'goose-cli', '--bin', 'goose', '--features', 'asb-native-probe']
    if offline:
        args.insert(2, '--offline')
    compact = None
    try:
        subprocess.run(args, cwd=source, env=environment, check=True)
        executable = target / 'release/goose'
        compact = symbols.prepare(executable, output.parent, symbol_root)
        check_environment = dict(environment, ASB_GOOSE_NATIVE_CAPABILITY='1')
        check = subprocess.run([str(compact), '--version'], env=check_environment,
                               stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                               stderr=subprocess.DEVNULL, timeout=5, check=False)
    except (OSError, subprocess.SubprocessError) as error:
        if compact is not None:
            compact.unlink(missing_ok=True)
        raise ValueError('Goose native build failed; install the Rust toolchain and build dependencies, then retry') from error
    if check.returncode != 73:
        compact.unlink(missing_ok=True)
        raise ValueError('Goose build did not provide the ASB native replay interface; preserve the build and retry')
    temporary = None
    try:
        output.parent.mkdir(parents=True, exist_ok=True)
        with tempfile.NamedTemporaryFile(prefix='.asb-goose-', dir=output.parent, delete=False) as destination:
            temporary = Path(destination.name)
            with compact.open('rb') as compiled:
                shutil.copyfileobj(compiled, destination, length=1024 * 1024)
            destination.flush()
            os.fsync(destination.fileno())
        temporary.chmod(0o755)
        os.replace(temporary, output)
    except OSError as error:
        raise ValueError('Cannot publish the Goose executable in the project tool directory; check available space and permissions') from error
    finally:
        compact.unlink(missing_ok=True)
        if temporary is not None:
            temporary.unlink(missing_ok=True)
    return output


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('source', type=Path)
    parser.add_argument('target', type=Path)
    parser.add_argument('output', type=Path)
    parser.add_argument('symbol_root', type=Path)
    parser.add_argument('--offline', action='store_true')
    args = parser.parse_args()
    try:
        build(args.source, args.target, args.output, args.symbol_root, offline=args.offline)
    except ValueError as error:
        print(f'ASB Goose build failed: {error}', file=sys.stderr)
        return 1
    print(f'Installed ASB-enabled Goose at {args.output}')
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
