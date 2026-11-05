#!/usr/bin/env python3
"""Optional offline native collector-loss/cancellation qualification.

Run inside a fresh delegated asb-pressure-*.service, as for delegated.py.
Only owned process generations in that service may receive a signal.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import runpy
import signal
import subprocess
import sys
import threading
import time
import overhead


def recording(arguments, project):
    """Select only this project's inherited SPE recording, never a host scan."""
    if (len(arguments) < 3 or arguments[1] != 'record' or arguments.count('-o') != 1
            or '--inherit' not in arguments or arguments.count('-p') != 1
            or any(arg in ['--no-inherit', '--per-thread'] or arg.startswith(('-a', '-C', '--cpu', '--all-cpus')) for arg in arguments)):
        return None
    target_index = arguments.index('-p') + 1
    if target_index >= len(arguments) or not arguments[target_index].isdigit() or not 0 < int(arguments[target_index]) <= 4294967295:
        return None
    index = arguments.index('-o') + 1
    if index >= len(arguments):
        return None
    path = Path(arguments[index]).resolve()
    project = project.resolve()
    parent = (project / 'results/pressure-runs').resolve()
    if not parent.is_relative_to(project):
        return None
    if path.name != 'arm-spe.data' or not path.is_relative_to(parent):
        return None
    return path


def identity(pid):
    text = Path('/proc/%d/stat' % pid).read_text()
    return int(text.rsplit(') ', 1)[1].split()[19])


def validate_loss(project, recording_path):
    """Recheck retained loss evidence offline, including independent grades/work."""
    runs = list(project.glob('results/pressure-runs/run-*/measurements-concurrency-*/run.json'))
    if len(runs) != 2 or any(overhead.read_json(path)['success_rate'] != 1 for path in runs):
        raise RuntimeError('collector loss changed the independent workload grades')
    checksums = []
    for replica in [1, 2]:
        log = overhead.read_json(project / ('pressure-concurrency-%d.log' % replica))
        markers = [line.split('=',1)[1] for line in log['stdout'].splitlines() if line.startswith('ASB_FIXED_WORK=')]
        if len(markers) != 1:
            raise RuntimeError('fixed-work completion proof is missing')
        proof = json.loads(markers[0])
        if (proof['iterations'] != 1000000000 or proof['children'] != 3
                or proof['io_blocks'] != 128 or proof['io_block_bytes'] != 1024 * 1024
                or len(proof['checksums']) != 3):
            raise RuntimeError('fixed work differs from the qualification budget')
        checksums.extend(proof['checksums'])
    if len(set(checksums)) != 1:
        raise RuntimeError('fixed-work checksums differ between replicas')
    documents = [json.loads(path.read_text()) for path in project.glob(
        'results/pressure-runs/run-*/measurements-concurrency-*/arm-spe.json')]
    if len(documents) != 2 or sorted(d['status'] for d in documents) != ['completed', 'failed']:
        raise RuntimeError('expected one completed and one failed SPE capture')
    failed = json.loads(recording_path.with_name('arm-spe.json').read_text())
    if failed['status'] != 'failed':
        raise RuntimeError('the interrupted recording did not retain its failed status')
    if failed['metrics'] or failed.get('series'):
        raise RuntimeError('lost capture published numeric SPE observations')
    analysis = json.loads(recording_path.with_name('bottlenecks.json').read_text())
    if any(metric['value'] is not None for group in analysis['sampling'] for metric in group['metrics']):
        raise RuntimeError('lost capture produced qualified sampling analysis')
    if any(item['status'] != 'unavailable' for item in analysis['phase_alignments']):
        raise RuntimeError('lost capture produced execution-aligned SPE claims')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--mode', required=True, choices=['collector-loss', 'cancel'])
    parser.add_argument('--asb', type=Path, required=True)
    parser.add_argument('--base-project', type=Path, required=True)
    parser.add_argument('--project', type=Path, required=True)
    parser.add_argument('--node', type=Path, required=True)
    args = parser.parse_args()
    membership = Path('/proc/self/cgroup').read_text().strip().split('::', 1)[1]
    service = Path('/sys/fs/cgroup') / membership.lstrip('/')
    if not service.name.startswith('asb-pressure-') or not service.name.endswith('.service'):
        parser.error('requires a fresh disposable delegated asb-pressure-*.service')
    binary = args.asb.resolve(strict=True)
    project = args.project.resolve()
    client_sha = overhead.stage(args.base_project.resolve(strict=True), project,
                                args.node.resolve(strict=True), 1000000000, 'spe')
    original = subprocess.Popen
    stop = threading.Event()
    digest = hashlib.sha256()
    with binary.open('rb') as executable:
        for chunk in iter(lambda: executable.read(256 * 1024), b''):
            digest.update(chunk)
    receipt = {'mode': args.mode, 'client_sha256': client_sha, 'binary_sha256': digest.hexdigest()}
    fault = None
    runner = None

    def inject(pid):
        try:
            deadline = time.monotonic() + 60
            while not stop.wait(0.05):
                leaves = list((service / 'agents').glob('*/cgroup.procs'))
                if len(leaves) >= 2 and all(len(p.read_text().split()) > 1 for p in leaves):
                    break
                if time.monotonic() > deadline:
                    raise RuntimeError('both owned agent trees did not start')
            if stop.is_set():
                raise RuntimeError('run ended before fault injection')
            time.sleep(0.25)
            candidates = [pid] if args.mode == 'cancel' else [
                int(value) for value in (service / 'control/cgroup.procs').read_text().split()]
            for selected in candidates:
                descriptor = None
                try:
                    descriptor = os.pidfd_open(selected)
                    generation = identity(selected)
                    members = set(map(int, (service / 'control/cgroup.procs').read_text().split()))
                    if selected not in members:
                        continue
                    if args.mode == 'collector-loss':
                        if Path('/proc/%d/exe' % selected).resolve().name != 'perf':
                            continue
                        with Path('/proc/%d/cmdline' % selected).open('rb') as command:
                            raw = command.read(65537)
                        if len(raw) > 65536:
                            continue
                        arguments = raw.rstrip(b'\0').decode().split('\0')
                        target = recording(arguments, project)
                        if target is None:
                            continue
                        agent_pid = int(arguments[arguments.index('-p') + 1])
                        agent_members = {int(value) for leaf in (service / 'agents').glob('*/cgroup.procs') for value in leaf.read_text().split()}
                        if agent_pid not in agent_members:
                            continue
                    else:
                        target = None
                    if identity(selected) != generation:
                        raise RuntimeError('owned target generation changed before signal')
                    sig = signal.SIGKILL if args.mode == 'collector-loss' else signal.SIGINT
                    signal.pidfd_send_signal(descriptor, sig)
                    receipt.update(signalled_pid=selected, start_time_ticks=generation,
                                   signal=int(sig), recording=str(target) if target else None,
                                   agent_target_pid=agent_pid if target else None)
                    return
                except (FileNotFoundError, ProcessLookupError):
                    continue
                finally:
                    if descriptor is not None:
                        os.close(descriptor)
            raise RuntimeError('no exact owned process was eligible for fault injection')
        except Exception as error:
            receipt['fault_error'] = str(error)

    def launch(arguments, *positional, **keywords):
        nonlocal fault, runner
        process = original(arguments, *positional, **keywords)
        if arguments[0] == str(binary):
            runner = process
            fault = threading.Thread(target=inject, args=(process.pid,))
            fault.start()
        return process

    driver = Path(__file__).parents[1] / 'kernel_pressure/delegated.py'
    sys.argv = [str(driver), '--asb', str(binary), '--project', str(project),
                '--config', 'pressure.toml']
    if args.mode == 'collector-loss':
        sys.argv += ['--require-cohort-analysis']
    subprocess.Popen = launch
    try:
        runpy.run_path(str(driver), run_name='__main__')
    except SystemExit as error:
        receipt['driver_exit'] = str(error.code)
    finally:
        subprocess.Popen = original
        stop.set()
        if fault:
            fault.join(timeout=5)
        receipt['runner_exit'] = runner.poll() if runner else None
        (project / 'fault-receipt.json').write_text(json.dumps(receipt, indent=2))
    if fault is None or fault.is_alive() or 'fault_error' in receipt or 'signalled_pid' not in receipt:
        raise RuntimeError('fault was not qualified; inspect fault-receipt.json')
    if args.mode == 'collector-loss':
        if receipt['runner_exit'] != 0 or 'driver_exit' in receipt:
            raise RuntimeError('collector loss prevented the independent agent run/scope qualification')
        validate_loss(project, Path(receipt['recording']))
        target = overhead.read_json(Path(receipt['recording']).with_name('measurement-target.json'))
        if target['root']['pid'] != receipt['agent_target_pid']:
            raise RuntimeError('the interrupted recorder did not belong to the measured actual agent root')
    else:
        if receipt['runner_exit'] in [None, 0]:
            raise RuntimeError('cancellation did not interrupt ASB')
        deadline = time.monotonic() + 10
        while True:
            remaining = {int(pid) for file in service.rglob('cgroup.procs')
                         for pid in file.read_text().split()} - {os.getpid()}
            if not remaining:
                break
            if time.monotonic() >= deadline:
                receipt['remaining_pids'] = sorted(remaining)
                (project / 'fault-receipt.json').write_text(json.dumps(receipt, indent=2))
                raise RuntimeError('cancelled agent/collector processes remained in the owned service')
            time.sleep(0.05)
        for result in project.glob('results/pressure-runs/run-*/measurements-concurrency-*/run.json'):
            if json.loads(result.read_text()).get('status') == 'completed':
                raise RuntimeError('cancelled execution was labelled completed')
    receipt['status'] = 'qualified'
    (project / 'fault-receipt.json').write_text(json.dumps(receipt, indent=2))
    print(json.dumps(receipt), flush=True)


if __name__ == '__main__':
    main()
