"""Publish a compact executable and separately indexed GNU debug information."""
import hashlib
import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile


def build_id(path):
    result = subprocess.run(['readelf', '-n', str(path)], check=True, stdout=subprocess.PIPE,
                            stderr=subprocess.PIPE, timeout=30, env=dict(os.environ, LC_ALL='C'))
    matches = re.findall(rb'^\s*Build ID:\s*([0-9a-f]+)\s*$', result.stdout, re.MULTILINE)
    if len(matches) != 1 or not 8 <= len(matches[0]) <= 128 or len(matches[0]) % 2:
        raise ValueError('Goose build has no unique GNU build ID')
    return matches[0].decode('ascii')


def digest(path):
    value = hashlib.sha256()
    with path.open('rb') as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b''):
            value.update(chunk)
    return value.digest()


def prepare(binary, directory, symbol_root):
    """Stage a compact binary and publish immutable, build-ID-matched symbols."""
    binary, directory, symbol_root = map(Path, (binary, directory, symbol_root))
    directory.mkdir(parents=True, exist_ok=True)
    symbol_root.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix='.goose-symbols-', dir=directory) as scratch:
        scratch = Path(scratch)
        compact, debug = scratch / 'goose', scratch / 'goose.debug'
        try:
            subprocess.run(['objcopy', '--only-keep-debug', '--compress-debug-sections=zlib',
                            str(binary), str(debug)], check=True, timeout=300)
            subprocess.run(['objcopy', '--strip-all', str(binary), str(compact)], check=True, timeout=300)
            identity = build_id(binary)
            if build_id(compact) != identity or build_id(debug) != identity:
                raise ValueError('Goose executable and debug information have different build IDs')
        except (OSError, subprocess.SubprocessError) as error:
            raise ValueError('Cannot create Goose profiling symbols; install binutils (objcopy and readelf) and retry') from error
        destination = symbol_root / '.build-id' / identity[:2] / (identity[2:] + '.debug')
        destination.parent.mkdir(parents=True, exist_ok=True)
        # Link a complete user-owned inode into place. Never overwrite an
        # existing build ID with different bytes, even on concurrent installs.
        with tempfile.NamedTemporaryFile(prefix='.goose-debug-', dir=destination.parent, delete=False) as stage:
            temporary = Path(stage.name)
            try:
                with debug.open('rb') as source:
                    shutil.copyfileobj(source, stage, length=1024 * 1024)
                stage.flush()
                os.fsync(stage.fileno())
            except BaseException:
                temporary.unlink(missing_ok=True)
                raise
        try:
            try:
                os.link(temporary, destination)
            except FileExistsError:
                if not destination.is_file() or destination.is_symlink() or digest(destination) != digest(temporary):
                    raise ValueError('Existing Goose symbols conflict with this build ID; preserve them and choose a clean tool directory')
        finally:
            temporary.unlink(missing_ok=True)
        descriptor, name = tempfile.mkstemp(prefix='.goose-profile-', dir=directory)
        os.close(descriptor)
        result = Path(name)
        try:
            os.replace(compact, result)
            result.chmod(0o755)
        except BaseException:
            result.unlink(missing_ok=True)
            raise
        return result
