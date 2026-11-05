#!/usr/bin/env python3
"""Opt-in fixed-work Arm instrumentation comparison; no online provider.

Use an existing disposable scope-fixture base with tools/perf and builtin
workloads. Every case gets a fresh project and transient delegated service.
"""
import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import pwd
import random
import shutil
import statistics
import subprocess
import sys
import time

PROFILES = {
    "light": ["system", "cpu-accounting", "kernel-pressure"],
    "pmu-perf": ["system", "cpu-accounting", "kernel-pressure", "arm-platform", "arm-pmu"],
    "spe": ["system", "cpu-accounting", "kernel-pressure", "arm-platform", "arm-pmu", "arm-spe"],
}
LIMITATIONS = ("Synthetic fixed-work client and local LLM replay, not real-agent/model performance. "
               "Profiles share the gated accounting/pressure baseline. PMU also enables perf stack recording; "
               "it is not counter-only. Unrelated negative-control noise is identical in every case. "
               "Randomized repeated observations do not establish causal overhead, production impact or a confidence interval. "
               "Agent phase and total CLI durations are different quantities; grading/decoding/reporting contribute only to the latter.")


def read_json(path):
    with path.open('rb') as stream:
        data = stream.read(8 * 1024 * 1024 + 1)
    if len(data) > 8 * 1024 * 1024:
        raise ValueError('qualification document exceeds input bound: ' + str(path))
    return json.loads(data)


def stage(base, project, node, iterations, profile):
    project.mkdir()  # Never replace an installed client or prior evidence.
    (project / '.asb').mkdir()
    (project / '.asb/project.toml').write_text('tools_dir="tools"\nworkloads_dir="workloads"\nworkspace_dir="workspaces"\n')
    shutil.copytree(base / 'workloads', project / 'workloads')
    executable = project / 'tools/pi/bin/pi'
    executable.parent.mkdir(parents=True)
    source = Path(__file__).with_name('fixed_work.js').read_text()
    if '\n' in str(node) or ' ' in str(node):
        raise ValueError('fixture requires an absolute Node executable path without whitespace')
    executable.write_text('#!' + str(node) + '\n' + source.replace('process.env.ASB_FIXED_WORK_ITERATIONS', str(iterations)))
    executable.chmod(0o755)
    (project / 'tools/perf').symlink_to((base / 'tools/perf').resolve(strict=True), target_is_directory=True)
    tape = {"request": {"method":"POST", "url":"/v1/responses", "body":'{"model":"test-model","input":"offline","tools":[{"type":"function"}]}'},
            "response": {"status":200,"headers":{"content-type":"application/json"},"body":'{"ok":true}'}}
    (project / 'tape.jsonl').write_text(json.dumps(tape) + '\n')
    (project / 'pressure.toml').write_text(
        'agent="pi"\nworkload="bug-fix"\nprovider="cli2key"\nmodel="test-model"\nconcurrency=2\n'
        'workspace="workspace"\nlog_file="pressure.log"\n[measurements]\nenabled=true\ndirectory="measurements"\ncollectors=' + json.dumps(PROFILES[profile]) + '\n')
    return hashlib.sha256(executable.read_bytes()).hexdigest()


def extract(project, profile, round_number, service_seconds, iterations):
    records = sorted(project.glob('results/pressure-runs/run-*/**/run.json'))
    if len(records) != 2:
        raise ValueError('expected exactly two measured replicas')
    scope = read_json(project / 'qualification-scope.json')
    cli_ns = scope['runner_wall_ns']
    if not isinstance(cli_ns, int) or isinstance(cli_ns, bool) or cli_ns <= 0:
        raise ValueError('actual CLI duration is unavailable')
    forbidden = {scope['runner_pid'], scope['fixture_pid'], scope['noise_pid']}
    replicas, populations = [], []
    for path in records:
        directory = path.parent
        run = read_json(path)
        target = read_json(directory / 'measurement-target.json')
        accounting = read_json(directory / 'cpu-accounting.json')
        if run['success_rate'] != 1 or accounting['status'] != 'completed' or accounting['source'] != 'cgroup-v2':
            raise ValueError('fixed work grade/accounting unavailable')
        forbidden.update([target['excluded_guardian']['pid'], target['excluded_controller_pid']])
        if profile != 'light':
            groups = read_json(directory / 'bottlenecks.json')['counter_groups']
            if not groups or any(g['status'] != 'qualified' or g['root'] != target['root'] for g in groups):
                raise ValueError('PMU groups were not qualified for the actual agent root')
        if profile == 'spe':
            quality = read_json(directory / 'arm-spe-quality.json')
            if quality['status'] != 'observed' or quality['root'] != target['root']:
                raise ValueError('SPE quality/root unavailable')
            populations.append(set(map(int, quality['raw_record_quality']['tasks'])))
        log = project / ('pressure-concurrency-%d.log' % (len(replicas) + 1))
        logged = read_json(log)
        markers = [line.split('=', 1)[1] for line in logged['stdout'].splitlines() if line.startswith('ASB_FIXED_WORK=')]
        if len(markers) != 1:
            raise ValueError('fixed-work completion proof is missing or ambiguous')
        proof = json.loads(markers[0])
        if (proof['iterations'] != iterations or proof['children'] != 3 or proof['io_blocks'] != 128
                or proof['io_block_bytes'] != 1024 * 1024 or len(proof['checksums']) != 3
                or len(set(proof['checksums'])) != 1):
            raise ValueError('fixed-work completion differs from the selected work budget')
        metrics = accounting['metrics']
        cpu_ms = metrics['cpu_user_ms'] + metrics['cpu_system_ms']
        if not math.isfinite(cpu_ms) or cpu_ms <= 0:
            raise ValueError('fixed work did not produce positive finite agent CPU accounting')
        replicas.append({'root':target['root'], 'agent_elapsed_ms':run['elapsed_ms'], 'cpu_ms':cpu_ms,'work_proof':proof})
    observed = {pid for row in scope['observations'] for leaf in row['leaves'].values() for pid in leaf}
    if forbidden & observed or any(r['root']['pid'] not in observed for r in replicas):
        raise ValueError('scope receipt includes ASB/noise or lacks the selected agent')
    if populations and (populations[0] & populations[1] or any(p & forbidden for p in populations)):
        raise ValueError('SPE populations overlap another replica or ASB/noise')
    cohort = read_json(project / 'results/pressure-comparison/cohort-analysis.json')['cohorts']
    if len(cohort) != 1 or cohort[0]['status'] != 'observed' or cohort[0]['peak_live_principals_lower_bound'] != 2:
        raise ValueError('both admitted replicas must have observed lifetime overlap')
    return {'profile':profile, 'round':round_number, 'cli_seconds':cli_ns / 1000000000, 'service_wall_seconds':service_seconds,
            'agent_cpu_ms_sum':sum(r['cpu_ms'] for r in replicas),
            'agent_phase_span_upper_ms':cohort[0]['agent_phase_span_upper_bound_ms'],
            'replicas':replicas, 'scope_receipt':str(project / 'qualification-scope.json')}


def summarize(rows):
    summaries = {}
    for profile in PROFILES:
        group = [r for r in rows if r['profile'] == profile]
        if not group:
            raise ValueError('missing instrumentation profile')
        summaries[profile] = {}
        for metric in ['agent_cpu_ms_sum', 'agent_phase_span_upper_ms', 'cli_seconds', 'service_wall_seconds']:
            values = [r[metric] for r in group]
            if any(not math.isfinite(v) or v <= 0 for v in values):
                raise ValueError('invalid observed duration')
            summaries[profile][metric] = {'count':len(values),'median':statistics.median(values),'min':min(values),'max':max(values)}
    for profile, metrics in summaries.items():
        for metric, observed in metrics.items():
            baseline = summaries['light'][metric]['median']
            observed['descriptive_change_from_light_pct'] = 100 * (observed['median'] / baseline - 1)
    return summaries


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--asb', required=True, type=Path)
    parser.add_argument('--base-project', required=True, type=Path)
    parser.add_argument('--output', required=True, type=Path)
    parser.add_argument('--node', type=Path, default=shutil.which('node'))
    parser.add_argument('--rounds', type=int, choices=range(3, 11), default=3)
    parser.add_argument('--iterations', type=int, default=250000000)
    args = parser.parse_args()
    if sys.platform != 'linux' or not args.node or not 1 <= args.iterations <= 1000000000:
        parser.error('requires Linux, Node and 1..1000000000 fixed iterations')
    binary, base, node = args.asb.resolve(strict=True), args.base_project.resolve(strict=True), Path(args.node).resolve(strict=True)
    output = args.output.resolve()
    output.mkdir()  # Existing evidence is never overwritten.
    driver = Path(__file__).parents[1] / 'kernel_pressure/delegated.py'
    user = pwd.getpwuid(os.getuid()).pw_name
    nonce = os.urandom(8).hex()
    binary_hash = hashlib.sha256(binary.read_bytes()).hexdigest()
    rows, order = [], []
    for round_number in range(1, args.rounds + 1):
        profiles = list(PROFILES)
        random.SystemRandom().shuffle(profiles)
        for profile in profiles:
            index = len(order) + 1
            project = output / ('case-%02d-%s' % (index, profile))
            client_hash = stage(base, project, node, args.iterations, profile)
            unit = 'asb-pressure-overhead-%s-%02d' % (nonce, index)
            command = ['sudo','-n','systemd-run','--wait','--pipe','--collect','--uid='+user,
                       '-p','Delegate=yes','-p','CPUAccounting=yes','-p','MemoryAccounting=yes','-p','IOAccounting=yes','-p','RuntimeMaxSec=150',
                       '--unit='+unit,sys.executable,str(driver),'--asb',str(binary),'--project',str(project),
                       '--config','pressure.toml','--require-cohort-analysis']
            if profile == 'spe':
                command += ['--require-series','arm-spe']
            order.append({'profile':profile,'round':round_number,'project':str(project),'client_sha256':client_hash,'command':command})
            state = {'schema_version':1,'binary_sha256':binary_hash,'iterations_per_child':args.iterations,
                     'order':order,'rows':rows,'status':'running','limitations':LIMITATIONS}
            (output / 'overhead.json').write_text(json.dumps(state,indent=2))
            print('Running owned transient scope: ' + json.dumps(command),flush=True)
            started = time.monotonic()
            with (project / 'qualification.log').open('x') as log:
                try:
                    result = subprocess.run(command,stdout=log,stderr=subprocess.STDOUT,timeout=180)
                except (subprocess.TimeoutExpired, KeyboardInterrupt):
                    stop = ['sudo','-n','systemctl','stop',unit]
                    print('Stopping timed-out owned scope: '+json.dumps(stop),flush=True)
                    subprocess.run(stop,check=True,timeout=20)
                    raise
            if result.returncode:
                raise RuntimeError('qualification failed; retain ' + str(project / 'qualification.log'))
            rows.append(extract(project,profile,round_number,time.monotonic()-started,args.iterations))
            print('Completed ' + profile + ' round ' + str(round_number),flush=True)
    if hashlib.sha256(binary.read_bytes()).hexdigest() != binary_hash or len({r['client_sha256'] for r in order}) != 1:
        raise RuntimeError('qualification inputs changed')
    checksums = {replica['work_proof']['checksums'][0] for row in rows for replica in row['replicas']}
    if len(checksums) != 1:
        raise RuntimeError('fixed work differed across profiles')
    state.update(rows=rows,summary=summarize(rows),status='completed')
    (output / 'overhead.json').write_text(json.dumps(state,indent=2))
    print(json.dumps(state['summary'],indent=2))


if __name__ == '__main__':
    main()
