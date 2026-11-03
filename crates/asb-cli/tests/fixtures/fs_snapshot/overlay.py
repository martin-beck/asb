"""Opt-in, offline four-replica copy-on-write Git workspace probe."""
import os
import pathlib
import subprocess
import shutil
import tempfile

project = pathlib.Path(__file__).resolve().parents[5]
root = project / 'target' / 'snapshot-fixtures'
root.mkdir(parents=True, exist_ok=True)
source = pathlib.Path(os.environ['ASB_SNAPSHOT_GIT_BASELINE']).resolve(strict=True)
fuse_overlay = pathlib.Path(os.environ['ASB_SNAPSHOT_FUSE_OVERLAYFS']).resolve(strict=True)
fusermount = pathlib.Path(os.environ['ASB_SNAPSHOT_FUSERMOUNT']).resolve(strict=True)
image = os.environ['ASB_SNAPSHOT_IMAGE']
if not (source / '.git').is_dir():
 raise RuntimeError('snapshot fixture requires an existing Git workspace')
d = pathlib.Path(tempfile.mkdtemp(dir=root, prefix='overlay-git-'))
success = False
try:
 for x in ('u0','u1','u2','u3','w0','w1','w2','w3','m0','m1','m2','m3'): (d/x).mkdir()
 script='''set -eu
pids=""
cleanup(){ for i in 0 1 2 3; do fusermount3 -u /work/m$i || true; done; for p in $pids; do wait "$p" || true; done; chmod -R a+rwX /work; }
trap cleanup EXIT
for i in 0 1 2 3; do /opt/fuse-overlayfs -f -o lowerdir=/source,upperdir=/work/u$i,workdir=/work/w$i /work/m$i >/work/log$i 2>&1 & pids="$pids $!"; done
for i in 0 1 2 3; do n=0; while ! mountpoint -q /work/m$i; do n=$((n+1)); [ "$n" -lt 100 ] || exit 42; sleep 0.05; done; done
mkdir /work/home
export HOME=/work/home
for i in 0 1 2 3; do git config --global --add safe.directory /work/m$i; done
python3 - <<'EOPY'
import concurrent.futures,hashlib,os,subprocess,time
from pathlib import Path
roots=[Path('/work/m'+str(i)) for i in range(4)]
initial=[(root/'.git/index').stat() for root in roots]
for field in ('st_dev','st_ino','st_mode','st_nlink','st_size','st_mtime_ns','st_ctime_ns'):
 print('initial_index_'+field+'_equal',len({getattr(value,field) for value in initial})==1)
def fingerprint(path): return hashlib.sha256(path.read_bytes()).hexdigest()
source=Path('/source/.git/index');original=fingerprint(source)
def probe(i):
 root=roots[i]; begin=time.monotonic()
 def git(*args):
  result=subprocess.run(['git','-c','safe.directory=*','-C',str(root),*args],stdout=subprocess.PIPE,stderr=subprocess.PIPE,timeout=90)
  if result.returncode: raise RuntimeError('git fixture failed: '+result.stderr.decode(errors='replace')[-300:])
  return result.stdout
 status=git('status','--porcelain=v2','--branch','--untracked-files=no')
 log=git('log','-3','--pretty=%h %s')
 return status,log,round(time.monotonic()-begin,3),fingerprint(root/'.git/index')
with concurrent.futures.ThreadPoolExecutor(max_workers=4) as pool: results=list(pool.map(probe,range(4)))
checks={
 'status_equal':len({r[0] for r in results})==1,
 'log_equal':len({r[1] for r in results})==1,
 'post_status_indexes_equal':len({r[3] for r in results})==1,
 'base_index_unchanged':fingerprint(source)==original,
}
def git_at(root,*args,input=None):
 result=subprocess.run(['git','-c','safe.directory=*','-C',str(root),*args],input=input,stdout=subprocess.PIPE,stderr=subprocess.PIPE,timeout=90)
 if result.returncode: raise RuntimeError('git fixture operation failed: '+result.stderr.decode(errors='replace')[-300:])
 return result.stdout
tracked=git_at(roots[0],'ls-files','-z').split(b'\\x00')
selected=None
for raw in tracked:
 if not raw: continue
 candidate=roots[0]/os.fsdecode(raw)
 if candidate.is_symlink() or not candidate.is_file() or candidate.stat().st_size>1048576: continue
 original_bytes=candidate.read_bytes()
 if b'\\x00' in original_bytes: continue
 clean=subprocess.run(['git','-c','safe.directory=*','-C',str(roots[0]),'diff','--quiet','--',os.fsdecode(raw)],stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
 if clean.returncode==0:
  selected=os.fsdecode(raw);break
if selected is None: raise RuntimeError('fixture requires one clean tracked text file')
base_file=Path('/source')/selected;base_hash=fingerprint(base_file)
with (roots[0]/selected).open('ab') as stream: stream.write(b'\\n')
patch=git_at(roots[0],'diff','--binary','--',selected)
if not patch: raise RuntimeError('fixture change did not produce a patch')
def apply_effect(root):
 git_at(root,'apply','--binary','-',input=patch)
 return fingerprint(root/selected),git_at(root,'diff','--binary','--',selected)
with concurrent.futures.ThreadPoolExecutor(max_workers=3) as pool: replica_effects=list(pool.map(apply_effect,roots[1:]))
checks['replayed_patch_contents_equal']=all(item[0]==fingerprint(roots[0]/selected) for item in replica_effects)
checks['replayed_patch_diffs_equal']=all(item[1]==patch for item in replica_effects)
checks['base_file_unchanged']=fingerprint(base_file)==base_hash
a=roots[0]/'asb-overlay-fixture-file';a.write_text('one');os.link(a,roots[0]/'asb-overlay-fixture-link');os.rename(a,roots[0]/'asb-overlay-fixture-renamed')
checks['hardlink_rename_preserved']=(roots[0]/'asb-overlay-fixture-link').stat().st_ino==(roots[0]/'asb-overlay-fixture-renamed').stat().st_ino
checks['fixture_isolated']=all(not (root/'asb-overlay-fixture-link').exists() for root in roots[1:]) and not Path('/source/asb-overlay-fixture-link').exists()
subprocess.run(['python3','-c','from pathlib import Path; Path(\"asb-child-fixture\").write_text(\"child\")'],cwd=roots[1],check=True)
checks['child_write_isolated']=(roots[1]/'asb-child-fixture').read_text()=='child' and all(not (root/'asb-child-fixture').exists() for root in (roots[0],roots[2],roots[3],Path('/source')))
for name,passed in checks.items(): print(name,passed)
print('git_probe_seconds',[r[2] for r in results])
if not all(checks.values()): raise RuntimeError('snapshot fixture checks failed')
EOPY
'''
 cmd=['docker','run','--rm','--pull=never','--network','none','--device','/dev/fuse','--cap-add','SYS_ADMIN','--security-opt','apparmor=unconfined','--mount',f'type=bind,src={d},dst=/work','--mount',f'type=bind,src={source},dst=/source,readonly','--mount',f'type=bind,src={fuse_overlay},dst=/opt/fuse-overlayfs,readonly','--mount',f'type=bind,src={fusermount},dst=/usr/bin/fusermount3,readonly','--entrypoint','/bin/sh',image,'-c',script]
 r=subprocess.run(cmd,text=True,capture_output=True,timeout=180);print(r.stdout,end='')
 if r.returncode: raise RuntimeError(f'fixture exit {r.returncode}; retained diagnostics: {d}; {r.stderr[-800:]}')
 success = True
finally:
 if success:
  shutil.rmtree(d)
