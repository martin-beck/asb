"""Disposable Goose CLI probe using only a loopback synthetic OpenAI server."""
import http.server
import concurrent.futures
import json
import os
import re
import signal
import shutil
import sqlite3
from pathlib import Path
import subprocess
import sys
import tempfile
import tarfile
import threading
import time

if os.geteuid() != 0 or len(sys.argv) != 2 or not os.environ.get('ASB_GOOSE_BINARY'):
    raise SystemExit('Run as root with ASB_GOOSE_BINARY set and one new disposable output directory')
asb = Path(__file__).resolve().parents[3]
os.environ.update(ASB_RUN_BROKER_PROBE='1', ASB_PROBE_BACKGROUND='1', ASB_PROBE_PREEXISTING='1')
root = Path(sys.argv[1]).resolve()
root.mkdir(parents=True, exist_ok=False)
counts = {'requests': 0, 'tool_selected': None, 'seen': []}

class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *args): pass
    def do_POST(self):
        request = json.loads(self.rfile.read(int(self.headers.get('content-length', '0'))))
        counts = self.server.counts
        counts['requests'] += 1
        tools = request.get('tools', [])
        names = [tool.get('function', {}).get('name') for tool in tools]
        counts['seen'].append((self.path, names))
        selected = next((name for name in names if name in ('write', 'developer__write')), None)
        delegate = next((name for name in names if name in ('delegate', 'summon__delegate')), None)
        background = os.environ.get('ASB_PROBE_BACKGROUND') == '1'
        if (os.environ.get('ASB_PROBE_DELEGATE') == '1' or background) and delegate and not counts.get('delegated'):
            counts['delegated'] = True
            delta = {'role':'assistant','tool_calls':[{'index':0,'id':'call_parent','type':'function','function':{'name':delegate,'arguments':json.dumps({'instructions':'Write probe.txt containing recorded fixture.','extensions':['developer'],'async':background})}}]}
            finish = 'tool_calls'
        elif background and delegate and not counts.get('loaded'):
            match = re.search(r'Task ([^\s]+) started in background', json.dumps(request.get('messages',[])))
            load = next((name for name in names if name in ('load','summon__load')), None)
            if match and load:
                counts['loaded'] = True
                delta = {'role':'assistant','tool_calls':[{'index':0,'id':'call_load','type':'function','function':{'name':load,'arguments':json.dumps({'source':match.group(1)})}}]}
                finish='tool_calls'
            else:
                delta={'role':'assistant','content':'fixture complete'};finish='stop'
        elif not counts['tool_selected'] and selected:
            counts['tool_selected'] = selected
            delta = {'role':'assistant','tool_calls':[{'index':0,'id':'call_fixture','type':'function','function':{'name':selected,'arguments':json.dumps({'path':'probe.txt','content':'recorded fixture'})}}]}
            finish = 'tool_calls'
        else:
            delta = {'role':'assistant','content':'fixture complete'}
            finish = 'stop'
        def chunk(value, reason=None):
            return {'id':'fixture','object':'chat.completion.chunk','created':1,'model':'fixture','choices':[{'index':0,'delta':value,'finish_reason':reason}]}
        payload = ('data: '+json.dumps(chunk(delta))+'\n\n'+'data: '+json.dumps(chunk({},finish))+'\n\n'+'data: [DONE]\n\n').encode()
        self.send_response(200);self.send_header('Content-Type','text/event-stream');self.send_header('Content-Length',str(len(payload)));self.end_headers();self.wfile.write(payload)

def start_provider():
    instance = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
    instance.counts = {'requests': 0, 'tool_selected': None, 'seen': []}
    worker = threading.Thread(target=instance.serve_forever, daemon=True);worker.start()
    return instance,worker

server,thread = start_provider()
server.counts = counts
env = os.environ.copy()
for key in list(env):
    if key.endswith('_API_KEY') or key.endswith('_AUTH_TOKEN'):
        env.pop(key, None)
env.update(HOME=str(root/'home'), GOOSE_PROVIDER='openai', GOOSE_MODEL='fixture',
           OPENAI_HOST=f'http://127.0.0.1:{server.server_port}', OPENAI_BASE_PATH='/v1/chat/completions',
           OPENAI_API_KEY='fixture-key', GOOSE_DISABLE_KEYRING='1', GOOSE_MODE='auto', GOOSE_DISABLE_SESSION_NAMING='true')
(root/'home').mkdir(exist_ok=True)
command = [str(Path(os.environ['ASB_GOOSE_BINARY']).resolve()),'run','--no-session','--no-profile','--with-builtin','developer','--output-format','json','-t','Write a file named probe.txt containing recorded fixture.']
if os.environ.get('ASB_PROBE_DELEGATE') == '1' or os.environ.get('ASB_PROBE_BACKGROUND') == '1':
    command[command.index('developer')] = 'developer,summon'
    command[-1] = 'Delegate the task of writing probe.txt to a child agent.'
try:
    if os.environ.get('ASB_RUN_BROKER_PROBE') != '1':
        with (root/'goose.log').open('w') as output:
            result = subprocess.run(command,cwd=root,env=env,stdout=output,stderr=subprocess.STDOUT,timeout=120)
        print('exit:', result.returncode, 'provider_requests:', counts['requests'], 'selected_tool:', counts['tool_selected'],
              'file_created:', (root/'probe.txt').exists(), 'seen:', counts['seen'])
    else:
        modules=str(asb/'assets/benchmarks')
        native=str(asb/'assets/scripts/native-tool-broker.mjs')
        boundary=str(asb/'assets/scripts/native-snapshot-boundary.mjs')
        harness=Path(__file__).resolve().parent
        workspace=root/'workspace';workspace.mkdir(exist_ok=True)
        marker=root/'marker.sock'; events=root/'events.json'; snapshot=root/'snapshot'; target=workspace/'probe.txt'
        children=[]
        snapshot_services=[]
        def launch(args, environment):
            p=subprocess.Popen(args,cwd=root,env=environment,stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True)
            children.append(p)
            if p.stdout.readline() != 'READY\n': raise RuntimeError('local fixture did not become ready')
            return p
        def stop(p):
            p.stdin.write('STOP\n');p.stdin.flush();p.stdin.close()
            if p.wait(timeout=120) != 0: raise RuntimeError('local fixture failed on shutdown')
        def tape(mode, cassette, run_workspace, suffix, upstream=None, id_map=None):
            ready=root/f'tape-{suffix}.json'
            logfile=(root/f'tape-{suffix}.log').open('w')
            args=[os.environ['ASB_TEST_NODE'],str(Path(native).with_name('llmtape.mjs')),mode,'--port','0']
            args += ['--upstream',upstream,'--out',str(cassette)] if mode=='record' else ['--cassette',str(cassette)]
            tape_env=env | {'ASB_TAPE_READY_FILE':str(ready),'ASB_TAPE_WORKSPACE':str(run_workspace),
                            'ASB_TAPE_AGENT_WIRE_EXTENSION':str(asb/'assets/scripts/goose-native-wire.mjs')}
            tape_env.pop('ASB_GOOSE_ID_MAP_FILE', None)
            p=subprocess.Popen(args,cwd=root,env=tape_env,stdin=subprocess.DEVNULL,stdout=logfile,stderr=subprocess.STDOUT)
            children.append(p)
            deadline=time.monotonic()+15
            while not ready.exists():
                if p.poll() is not None: raise RuntimeError('llmtape failed to start')
                if time.monotonic()>=deadline: raise RuntimeError('llmtape readiness timed out')
                time.sleep(.02)
            return p,json.loads(ready.read_text())['port']
        def stop_tape(p):
            p.send_signal(signal.SIGTERM)
            if p.wait(timeout=15) != 0:raise RuntimeError('llmtape failed to stop cleanly')
        try:
            guest = str(Path(modules) / 'asb_snapshot_guest.py')
            def snapshot_service(workspace, marker, label, archive=None):
                args=['python3',guest,'--target',str(workspace),'--root',str(root/(label+'-storage')),'--socket',str(marker),'--output',str(root/(label+'-streams')),'--btrfs','/usr/bin/btrfs','--mkfs','/usr/sbin/mkfs.btrfs','--rsync','/usr/bin/rsync','--size',str(512*1024*1024),'--apply-final']
                if archive: args += ['--replay-archive',str(archive)]
                service=launch(args,env)
                snapshot_services.append(service)
                return service
            service=snapshot_service(workspace,marker,'record')
            broker_env=env | {'ASB_TEST_MARKER':str(marker),'ASB_TEST_BOUNDARY_MODULE':boundary}
            record_socket=root/'record.sock'
            record=launch([os.environ['ASB_TEST_NODE'],str(harness/'asb_broker_probe.mjs'),native,'record',str(record_socket),str(events),str(snapshot),str(target)],broker_env)
            cassette=root/'llm.jsonl'
            record_tape,record_port=tape('record',cassette,workspace,'record',f'http://127.0.0.1:{server.server_port}')
            with (root/'record.log').open('w') as output:
                a=subprocess.run(command,cwd=workspace,env=env | {'OPENAI_HOST':f'http://127.0.0.1:{record_port}','ASB_GOOSE_PROBE_SOCKET':str(record_socket),'ASB_GOOSE_PROBE_MODE':'record','ASB_GOOSE_PROBE_MARKER':str(marker)},stdout=output,stderr=subprocess.STDOUT,timeout=120)
            stop_tape(record_tape);stop(record)
            stop(service)
            archive=root/'snapshot-streams.tar'
            with tarfile.open(archive,'x') as output:
                for path in sorted((root/'record-streams').iterdir()):output.add(path,arcname=path.name,recursive=False)
            recorded_requests=counts['requests']; recorded_file=target.read_text() if target.exists() else None
            server.shutdown();server.server_close();thread.join()
            replicas=[]; tape_processes=[]
            for index in range(1,5):
                replay_workspace=root/f'w{index}'
                replay_workspace.mkdir(exist_ok=True)
                replay_target=replay_workspace/'probe.txt'
                pass
                replay_marker=root/f'm{index}.sock'
                replay_service=snapshot_service(replay_workspace,replay_marker,f'replay-{index}',archive)
                replay_socket=root/f'r{index}.sock'
                replay_env=env | {'ASB_TEST_MARKER':str(replay_marker),'ASB_TEST_BOUNDARY_MODULE':boundary}
                replay_broker=launch([os.environ['ASB_TEST_NODE'],str(harness/'asb_broker_probe.mjs'),native,'replay',str(replay_socket),str(events),str(snapshot),str(replay_target)],replay_env)
                replay_cassette=root/f'llm-{index}.jsonl';shutil.copyfile(cassette,replay_cassette)
                id_map=root/f'id{index}.json'
                proxy,proxy_port=tape('replay',replay_cassette,replay_workspace,f'replay-{index}',id_map=id_map);tape_processes.append(proxy)
                home=root/f'home{index}';home.mkdir()
                if os.environ.get('ASB_PROBE_PREEXISTING')=='1':
                    source=root/'home/.local/share/goose/sessions/sessions.db'
                    dest=home/'.local/share/goose/sessions/sessions.db';dest.parent.mkdir(parents=True)
                    with sqlite3.connect(f'file:{source}?mode=ro',uri=True) as src, sqlite3.connect(dest) as dst:src.backup(dst)
                run_env=env | {'HOME':str(home),'OPENAI_HOST':f'http://127.0.0.1:{proxy_port}', 'ASB_GOOSE_PROBE_SOCKET':str(replay_socket),'ASB_GOOSE_PROBE_MODE':'replay','ASB_GOOSE_ID_MAP_FILE':str(id_map)}
                if os.environ.get('ASB_TEST_SKIP_MAP_URL') != '1':
                    run_env['ASB_GOOSE_ID_MAP_URL']=f'http://127.0.0.1:{proxy_port}/.asb/goose-session-map'
                replicas.append((index,replay_workspace,replay_target,replay_broker,replay_service,proxy,run_env))
            def run_replica(entry):
                index,run_workspace,_,_,_,_,run_env=entry
                with (root/f'replay-{index}.log').open('w') as output:
                    return subprocess.run(command,cwd=run_workspace,env=run_env,stdout=output,stderr=subprocess.STDOUT,timeout=120).returncode
            try:
                with concurrent.futures.ThreadPoolExecutor(max_workers=4) as pool:
                    codes=list(pool.map(run_replica,replicas))
            finally:
                for proxy in tape_processes:stop_tape(proxy)
            for _,_,_,broker,service,_,_ in replicas:stop(broker);stop(service)
            def response(phase):
                text=(root/(phase+'.log')).read_text(); data=json.loads(text[text.index('{'):])
                values=[item['toolResult'] for message in data['messages'] for item in message.get('content',[]) if item.get('type')=='toolResponse']
                expected_count=2 if os.environ.get('ASB_PROBE_BACKGROUND')=='1' else 1
                if len(values)!=expected_count:raise RuntimeError(f'fixture expected {expected_count} tool result(s)')
                return values
            def canonical(value, mapping):
                if isinstance(value,str):
                    for actual,recorded in mapping.items():value=value.replace(actual,recorded)
                    return re.sub(r'(\*\*Duration:\*\* )\d+s',r'\1<duration>s',value)
                if isinstance(value,list):return [canonical(item,mapping) for item in value]
                if isinstance(value,dict):return {key:(0 if key=='duration_secs' and isinstance(item,int) else canonical(item,mapping)) for key,item in value.items()}
                return value
            expected=response('record')
            progress=[json.loads((root/f'llm-{index}.jsonl.progress.json').read_text()) for index in range(1,5)]
            def equivalent(item):
                mapping_file=root/f'id{item[0]}.json'
                mappings=json.loads(mapping_file.read_text()) if mapping_file.exists() else {}
                mapping={live:recorded for scope in mappings.values() for live,recorded in scope.items()}
                return canonical(response(f'replay-{item[0]}'),mapping)==canonical(expected,{})
            if a.returncode or codes!=[0,0,0,0] or recorded_file!='recorded fixture' or any(item[2].read_text()!='recorded fixture' or not equivalent(item) for item in replicas) or any(item['consumed']!=item['total'] or item['rejected']!=0 for item in progress):
                raise RuntimeError('local Goose record/replay did not reproduce the result and effect')
            print('record exit:',a.returncode,'replay exits:',codes,'record requests:',recorded_requests,
                  'provider stopped before replay:',True,'llmtape consumed/total:',[(item['consumed'],item['total']) for item in progress], 'matching results and effects:',True)
        finally:
            # The supervisor owns bind mounts and loop devices. Give it a
            # chance to settle and detach them even if a fixture assertion
            # failed; forceful termination alone would leak those resources.
            for p in snapshot_services:
                if p.poll() is None:
                    try:
                        stop(p)
                    except (OSError, subprocess.SubprocessError, RuntimeError):
                        pass
            for p in children:
                if p.poll() is None:
                    p.terminate()
                    try:p.wait(timeout=5)
                    except subprocess.TimeoutExpired:p.kill();p.wait()
finally:
    server.shutdown();server.server_close();thread.join()
