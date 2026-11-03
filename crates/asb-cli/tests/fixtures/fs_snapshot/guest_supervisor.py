"""Opt-in offline snapshot supervisor probe in a real task container."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import uuid

import docker

ROOT=Path(__file__).resolve().parents[5]
sys.path.insert(0,str(ROOT/'crates/asb-cli/assets/benchmarks'))
from asb_execution import ManagedDockerWorkspace
from asb_guest import stage_auxiliary_tools
from asb_execution import ExecutionError
from asb_snapshot_control import GuestSnapshot

scratch=ROOT/'target/snapshot-supervisor-probe';scratch.mkdir(parents=True,exist_ok=True)
tools={'btrfs':os.environ['ASB_SNAPSHOT_BTRFS'],
       'mkfs.btrfs':os.environ['ASB_SNAPSHOT_MKFS_BTRFS'],
       'rsync':os.environ['ASB_SNAPSHOT_RSYNC']}
client=docker.from_env(timeout=60)
try:
 with tempfile.TemporaryDirectory(dir=scratch) as directory:
  image=client.images.get(os.environ['ASB_SNAPSHOT_IMAGE'])
  with ManagedDockerWorkspace(client,image,'asb-'+uuid.uuid4().hex,'/testbed',snapshot=True) as workspace:
   container=workspace.container
   def checked(command):
    result=container.exec_run(command)
    if result.exit_code:raise RuntimeError('guest setup failed')
    return result.output
   checked(['mkdir','-p','/opt/asb-private'])
   task=type('Task',(),{'workspace':workspace,'artifacts':Path(directory),'checked':staticmethod(checked)})()
   paths=stage_auxiliary_tools(task,tools,(0,0))
   supervisor=GuestSnapshot(task,paths,(0,0),['docker'],time.monotonic()+300)
   try:
    program=('import socket,pathlib\n'
             'def mark(action):\n'
             ' s=socket.socket(socket.AF_UNIX);s.connect("/opt/asb-private/snapshot.sock");'
             's.sendall(("tool-"+action+":fixture\\n").encode());'
             'assert s.makefile("rb").readline().strip().isdigit();s.close()\n'
             'mark("begin");pathlib.Path("/testbed/asb-fixture").write_text("fixed");mark("end")\n')
    result=container.exec_run(['python3','-c',program])
    if result.exit_code:raise RuntimeError('guest tool boundary failed')
    try:supervisor.close()
    except ExecutionError as error:
     if 'cassette publication is not yet integrated' not in str(error):raise
    else:raise RuntimeError('incomplete cassette unexpectedly accepted')
    if not (Path(directory)/'snapshot-streams/snapshot-00000002.btrfs').is_file():
     raise RuntimeError('snapshot stream missing from controller artifacts')
    if container.exec_run(['test','-s','/testbed/asb-fixture']).exit_code:
     raise RuntimeError('final workspace changes did not return to task checkout')
    print('guest supervisor captured and exported tool boundary')
   finally:
    if supervisor.process is not None:supervisor.abort()
finally:
 client.close()
