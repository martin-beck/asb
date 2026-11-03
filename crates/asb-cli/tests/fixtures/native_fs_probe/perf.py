"""Measure Git status through the experimental proxy on a real checkout."""
import os
from pathlib import Path
import subprocess
import tempfile
import time

HERE = Path(__file__).resolve().parent
BASE = HERE.parents[4] / 'target/native-fs-probe'
BASE.mkdir(parents=True, exist_ok=True)
python = os.environ['ASB_FUSE_TEST_PYTHON']
source = Path(os.environ['ASB_SWE_TEST_BASELINE']).resolve()
with tempfile.TemporaryDirectory(dir=BASE) as temp:
    root = Path(temp)
    backing, mount, evidence = (root / name for name in ('backing','mount','evidence'))
    for path in (backing,mount,evidence): path.mkdir()
    subprocess.run(['cp','-a','--reflink=auto',str(source)+'/.',str(backing)],check=True)
    env = dict(os.environ, ASB_PROBE_ROOT_TGID=str(os.getpid()))
    warm = env.get('ASB_FUSE_PERF_WARM') == '1'
    if warm: env.pop('GIT_OPTIONAL_LOCKS',None)
    else: env['GIT_OPTIONAL_LOCKS']='0'
    def status(path):
        start=time.monotonic()
        result=subprocess.run(['git','-C',str(path),'status','--porcelain=2','--branch'],env=env,stdout=subprocess.PIPE,stderr=subprocess.PIPE,check=True,timeout=120)
        return time.monotonic()-start,result.stdout
    direct,direct_out=status(backing)
    process=subprocess.Popen([python,str(HERE.parents[2]/'assets/benchmarks/asb_fs_proxy.py'),str(backing),str(mount),str(evidence)],env=env,stdout=subprocess.DEVNULL,stderr=subprocess.PIPE)
    mounted=False
    try:
        deadline=time.monotonic()+5
        while not os.path.ismount(mount):
            if process.poll() is not None or time.monotonic()>deadline: raise RuntimeError('FUSE mount failed')
            time.sleep(.01)
        mounted=True
        through,through_out=status(mount)
        assert through_out==direct_out, 'Git status changed through the proxy'
        if warm:
            warm_time,warm_out=status(mount)
            assert warm_out==direct_out, 'warm Git status changed through the proxy'
            print(f'second proxy Git status: {warm_time:.3f}s',flush=True)
    finally:
        if mounted: subprocess.run(['fusermount3','-u',str(mount)],check=True)
        try: process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            process.terminate(); process.wait(timeout=5)
    if process.returncode: raise RuntimeError('FUSE process failed')
    event_count=sum(1 for _ in (evidence/'events.jsonl').open())
    object_count=sum(1 for p in evidence.iterdir() if p.name.startswith('object-'))
    identity_mode='disabled for benchmark only' if env.get('ASB_PROBE_BENCHMARK_NO_IDENTITY')=='1' else 'enabled'
    print(f'git status: direct={direct:.3f}s, proxy={through:.3f}s, events={event_count}, payload_files={object_count}, attribution={identity_mode}')
