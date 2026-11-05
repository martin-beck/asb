"""Opt-in full ASB/Pi snapshot cassette qualification on one original SWE task.

Uses a synthetic loopback provider and an explicit upstream reference patch;
this is not an AI-solving or benchmark-performance claim. Run only in a
disposable VM with the official catalog, task image and Pi assets installed.
"""
import base64
import http.server
import json
import os
from pathlib import Path
import shlex
import subprocess
import sys
import tempfile
import threading

HERE=Path(__file__).resolve().parent
PROJECT=HERE.parents[2]
sys.path.insert(0,str(PROJECT/'assets/benchmarks'))
sys.path.insert(0,str(PROJECT/'tests'))
from asb_swebench import read_task
import guest_provider_fixture as provider


def main():
    root=Path(os.environ['ASB_TEST_SWECATALOG_ROOT']).resolve(strict=True)
    binary=Path(os.environ['ASB_TEST_BENCHMARK_ASB']).resolve(strict=True)
    if not (root/'.asb/project.toml').is_file():raise RuntimeError('fixture requires an initialized ASB project')
    task='sympy__sympy-22236'
    _,row=read_task(root/'workloads/swe-bench/catalog.json',task)
    patch=row.get('patch')
    if not isinstance(patch,str) or not patch:raise RuntimeError('official reference patch is unavailable')
    resume=os.environ.get('ASB_TEST_RESUME_RECORDING')
    artifacts=(Path(resume).resolve(strict=True) if resume else
               Path(tempfile.mkdtemp(prefix='pi-swe-snapshot-c4-',dir=root)))
    if resume and (artifacts.parent != root or not (artifacts/'record.asb.zip').is_file()):
        raise RuntimeError('resume requires retained fixture evidence in this project')
    print('fixture evidence:',artifacts,flush=True)
    workdir=Path(tempfile.mkdtemp(prefix='pi-swe-config-',dir=root/'results'))
    provider.root=workdir
    provider.requests=provider.calls=0
    calls=[]
    class DataHandler(http.server.BaseHTTPRequestHandler):
        def log_message(self,*_args):pass
        def do_GET(self):
            calls.append(1)
            payload=b'fixture-http'
            self.send_response(200);self.send_header('Content-Length',str(len(payload)))
            self.end_headers();self.wfile.write(payload)
    data=http.server.ThreadingHTTPServer(('0.0.0.0',0),DataHandler)
    llm=http.server.ThreadingHTTPServer(('127.0.0.1',0),provider.Handler)
    for server in [data,llm]:threading.Thread(target=server.serve_forever,daemon=True).start()
    gateway=subprocess.check_output(['docker','network','inspect','bridge','--format','{{(index .IPAM.Config 0).Gateway}}'],text=True).strip()
    if not gateway:raise RuntimeError('Docker bridge has no guest gateway')
    encoded=base64.b64encode(patch.encode()).decode('ascii')
    apply='import base64,subprocess; subprocess.run(["git","apply","-"],input=base64.b64decode('+repr(encoded)+'),check=True)'
    (workdir/'command.txt').write_text('curl -fsS --max-time 10 '+shlex.quote(f'http://{gateway}:{data.server_port}/')+
                                          ' && python -c '+shlex.quote(apply))
    configs=[]
    for concurrency in [1,4]:
        config=workdir/f'c{concurrency}.toml'
        config.write_text("agent='pi'\nprovider='openrouter'\nmodel='fixture-model'\n"
            f"workload='swe-bench'\ntrial='{task}'\nconcurrency={concurrency}\n"
            "workspace='unused-workspace'\nlog_file='unused.log'\n"
            "[measurements]\nenabled=false\ndirectory='unused-measurements'\ncollectors=[]\n")
        configs.append(config)
    base=f'http://127.0.0.1:{llm.server_port}/v1'
    env={'PATH':os.environ['PATH'],'HOME':str(artifacts),'OPENROUTER_API_KEY':'fixture-token',
         'OPENROUTER_BASE_URL':base,'OPENROUTER_MODELS_URL':base+'/models',
         'NO_PROXY':'*','no_proxy':'*','NPM_CONFIG_OFFLINE':'true',
         'ASB_EXPERIMENTAL_SNAPSHOT':'1','ASB_EXPERIMENTAL_SNAPSHOT_ROOTS':'["/tmp"]'}
    cassette=artifacts/'record.asb.zip'
    def run(phase,config):
        args=[str(binary),'run',str(config),'--'+phase,'--repeat','1','--cassette',str(cassette),'--json']
        if phase=='replay':args+=['--track','1']
        try:
            result=subprocess.run(args,cwd=root,env=env,capture_output=True,text=True,timeout=1800)
        except subprocess.TimeoutExpired as error:
            (artifacts/f'{phase}.timeout.txt').write_text(str(error))
            raise
        (artifacts/f'{phase}.stdout').write_text(result.stdout)
        (artifacts/f'{phase}.stderr').write_text(result.stderr)
        if result.returncode:raise RuntimeError(f'{phase} failed ({result.returncode}); inspect {artifacts}/{phase}.stderr')
        lines=[json.loads(line) for line in result.stdout.splitlines() if line.strip()]
        rows=[row for row in lines if isinstance(row.get('task'),dict)]
        expected=1 if phase=='record' else 4
        if len(rows)!=expected:raise RuntimeError(f'{phase} emitted {len(rows)} outcomes instead of {expected}')
        for row in rows:
            outcome=row.get('benchmark_outcome') or {}
            if not (row.get('process_success') is True and row.get('workload_success') is True
                    and outcome.get('execution_success') is True and outcome.get('cleanup_confirmed') is True):
                raise RuntimeError(f'{phase} did not fully resolve and clean up; inspect retained output')
        if phase=='record':
            publications=[line['publication'] for line in lines if isinstance(line.get('publication'),dict)]
            if (len(publications)!=1 or publications[0].get('status')!='published'
                or publications[0].get('cleanup_complete') is not True or publications[0].get('warnings')):
                raise RuntimeError('record did not publish a clean complete cassette')
        return rows
    try:
        if resume:
            lines=[json.loads(line) for line in (artifacts/'record.stdout').read_text().splitlines() if line.strip()]
            outcomes=[line for line in lines if isinstance(line.get('task'),dict)]
            publications=[line['publication'] for line in lines if isinstance(line.get('publication'),dict)]
            if (len(outcomes)!=1 or outcomes[0].get('workload_success') is not True
                or (outcomes[0].get('benchmark_outcome') or {}).get('cleanup_confirmed') is not True
                or len(publications)!=1 or publications[0].get('cleanup_complete') is not True
                or publications[0].get('status')!='published'):
                raise RuntimeError('retained record lacks a successful grade and confirmed publication cleanup')
            print('resuming the retained successful recording without rerunning the agent',flush=True)
        else:
            run('record',configs[0])
            if len(calls)!=1 or provider.calls<2:raise RuntimeError('record did not contact both local fixture services')
            print('record succeeded with one local shell HTTP request and official grade',flush=True)
        data.shutdown();data.server_close();llm.shutdown();llm.server_close()
        run('replay',configs[1])
        if len(calls)!=(0 if resume else 1):raise RuntimeError('replay repeated the shell HTTP request')
        print('four Pi snapshot cassette replays completed with official grades and no fixture service',flush=True)
    finally:
        for server in [data,llm]:
            try:server.server_close()
            except OSError:pass
        print('fixture evidence:',artifacts,flush=True)


if __name__=='__main__':main()
