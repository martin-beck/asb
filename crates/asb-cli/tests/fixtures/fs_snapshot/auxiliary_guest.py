"""Opt-in, offline validation of bundled filesystem tools in a task image."""
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import uuid

ROOT=Path(__file__).resolve().parents[5]
sys.path.insert(0,str(ROOT/'crates/asb-cli/assets/benchmarks'))
from asb_runtime import bundle_tool

scratch=ROOT/'target/snapshot-auxiliary-test'
scratch.mkdir(parents=True,exist_ok=True)
image=os.environ['ASB_SNAPSHOT_IMAGE']
tools={'btrfs':os.environ['ASB_SNAPSHOT_BTRFS'],
       'mkfs.btrfs':os.environ['ASB_SNAPSHOT_MKFS_BTRFS'],
       'rsync':os.environ['ASB_SNAPSHOT_RSYNC']}
container='asb-aux-probe-'+uuid.uuid4().hex
with tempfile.TemporaryDirectory(dir=scratch) as directory:
    root=Path(directory)
    for tool,source in tools.items():
        files,_=bundle_tool(source,tool,root='/opt/asb-private')
        for name,data,mode in files:
            target=root/name;target.parent.mkdir(parents=True,exist_ok=True)
            target.write_bytes(data);target.chmod(mode)
    try:
        subprocess.run(['docker','run','-d','--name',container,'--pull=never','--network','none',
                        '--entrypoint','/bin/sh',image,'-c','exec tail -f /dev/null'],
                       stdout=subprocess.DEVNULL,check=True,timeout=30)
        subprocess.run(['docker','exec',container,'mkdir','-p','/opt/asb-private'],check=True,timeout=30)
        subprocess.run(['docker','cp',str(root)+ '/.',container+':/opt/asb-private'],check=True,timeout=60)
        for tool in tools:
            command=f'/opt/asb-private/tools/{tool}/{tool}'
            result=subprocess.run(['docker','exec',container,command,'--version'],capture_output=True,text=True,timeout=30)
            if result.returncode or not result.stdout.strip():
                raise RuntimeError(f'{tool} did not execute in the task image: {result.stderr[-500:]}')
            print(tool,result.stdout.splitlines()[0])
    finally:
        subprocess.run(['docker','rm','-f',container],stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL,timeout=30)
