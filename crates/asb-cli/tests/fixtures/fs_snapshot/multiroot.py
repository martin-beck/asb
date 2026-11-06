"""Opt-in Btrfs multi-root record plus four isolated replay replicas.

Run as root inside a private mount namespace; ASB_MULTIROOT_FIXTURE_DIR must
name a new disposable directory on the project disk. This is a filesystem
fixture, not an agent or complete-filesystem qualification.
"""
from concurrent.futures import ThreadPoolExecutor
import os
from pathlib import Path
import socket
import subprocess
import sys
import tarfile

PROJECT = Path(__file__).resolve().parents[5]
GUEST = PROJECT/'crates/asb-cli/assets/benchmarks/asb_snapshot_guest.py'
ROOT = Path(os.environ['ASB_MULTIROOT_FIXTURE_DIR']).resolve()
TARGET, TEMPORARY = ROOT/'task', ROOT/'temporary'


def start(label, archive=None):
    directory = ROOT/label
    directory.mkdir()
    args = [sys.executable, str(GUEST), '--target', str(TARGET), '--extra-root', str(TEMPORARY),
            '--root', str(directory/'storage'), '--socket', str(directory/'marker'),
            '--output', str(directory/'streams'), '--btrfs', '/usr/bin/btrfs',
            '--mkfs', '/usr/sbin/mkfs.btrfs', '--rsync', '/usr/bin/rsync',
            '--size', str(512*1024*1024)]
    args += ['--replay-archive', str(archive)] if archive else ['--apply-final']
    log = (directory/'error.log').open('wb')
    process = subprocess.Popen(args, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=log, text=True)
    log.close()
    if process.stdout.readline() != 'READY\n':
        process.wait(timeout=30)
        raise RuntimeError(f'{label} did not start; inspect {directory}/error.log')
    return directory, process


def marker(directory, action):
    with socket.socket(socket.AF_UNIX) as connection:
        connection.settimeout(60)
        connection.connect(str(directory/'marker'))
        connection.sendall(f'tool-{action}:fixture\n'.encode())
        if not connection.recv(128).strip().isdigit():
            raise RuntimeError('snapshot boundary failed')


def stop(process):
    output, _ = process.communicate('STOP\n', timeout=180)
    if process.returncode or 'SAVED\n' not in output:
        raise RuntimeError('snapshot supervisor did not settle')


def replica(label):
    directory, process = start(label, ROOT/'snapshots.tar')
    try:
        assert (TARGET/'file').read_text() == 'before'
        assert (TEMPORARY/'file').read_text() == 'before'
        marker(directory, 'end')
        assert (TARGET/'file').read_text() == 'workspace after'
        assert (TEMPORARY/'file').read_text() == 'temporary after'
        (TEMPORARY/'replica').write_text(label)
        stop(process)
    finally:
        if process.poll() is None:
            process.communicate('STOP\n', timeout=180)


def main():
    if os.geteuid() != 0:
        raise SystemExit('Run as root in a private mount namespace')
    if len(sys.argv) == 3 and sys.argv[1] == '--replica':
        replica(sys.argv[2])
        return
    ROOT.mkdir(exist_ok=False)
    TARGET.mkdir(); TEMPORARY.mkdir()
    (TARGET/'file').write_text('before'); (TEMPORARY/'file').write_text('before')
    directory, process = start('record')
    try:
        marker(directory, 'begin')
        (TARGET/'file').write_text('workspace after')
        (TEMPORARY/'file').write_text('temporary after')
        marker(directory, 'end')
        stop(process)
    finally:
        if process.poll() is None:
            process.communicate('STOP\n', timeout=180)
    with tarfile.open(ROOT/'snapshots.tar', 'x') as archive:
        for path in sorted((directory/'streams').iterdir()):
            archive.add(path, arcname=path.name, recursive=False)
    def run(index):
        subprocess.run(['unshare', '--mount', '--propagation', 'private', sys.executable,
                        __file__, '--replica', f'replay-{index}'], check=True)
    with ThreadPoolExecutor(max_workers=4) as pool:
        list(pool.map(run, range(4)))
    assert not (TEMPORARY/'replica').exists()
    print('Multi-root record and four isolated offline replays passed:', ROOT)


if __name__ == '__main__':
    main()
