import json, os, pathlib, signal, subprocess, sys, time
asb=pathlib.Path(__file__).resolve().parents[3]
sys.path.insert(0,str(asb/'assets/benchmarks'))
from asb_relay import HostRelay
required=('ASB_RELAY_BINARY','ASB_TEST_NODE','ASB_TEST_CASSETTE','ASB_TEST_IMAGE','ASB_TEST_LIB_DIR','ASB_TEST_LOADER')
if len(sys.argv)!=2 or any(not os.environ.get(name) for name in required):
  raise SystemExit('Provide a new output directory and the ASB_TEST_* and ASB_RELAY_BINARY variables described in README')
root=pathlib.Path(sys.argv[1]).resolve();root.mkdir(parents=True,exist_ok=False)
cassette=root/'tape.jsonl'
source=pathlib.Path(os.environ['ASB_TEST_CASSETTE']).resolve()
cassette.write_bytes(source.read_bytes())
node=os.environ['ASB_TEST_NODE']
scripts=asb/'assets/scripts'
env=os.environ.copy();env.update(ASB_TAPE_READY_FILE=str(root/'ready.json'),ASB_TAPE_AGENT_WIRE_EXTENSION=str(scripts/'goose-native-wire.mjs'))
log=(root/'proxy.log').open('w')
proxy=subprocess.Popen([node,str(scripts/'llmtape.mjs'),'replay','--port','0','--cassette',str(cassette)],env=env,stdout=log,stderr=subprocess.STDOUT)
container=None;relay=None
try:
  deadline=time.monotonic()+15
  while not (root/'ready.json').exists():
    if proxy.poll() is not None or time.monotonic()>deadline:raise RuntimeError('proxy failed to start')
    time.sleep(.05)
  port=json.loads((root/'ready.json').read_text())['port']
  image=os.environ['ASB_TEST_IMAGE']
  container=subprocess.check_output(['docker','run','-d','--network','none','--entrypoint','/bin/sh','-v',os.environ['ASB_TEST_LIB_DIR']+':/host-lib:ro','-v',os.environ['ASB_RELAY_BINARY']+':/opt/asb:ro',image,'-c','sleep 120'],text=True).strip()
  relay=HostRelay([('127.0.0.1',port)],['docker','exec','-i',container,'/host-lib/'+os.environ['ASB_TEST_LOADER'],'--library-path','/host-lib','/opt/asb','__asb-relay'])
  code='import urllib.request,json; u="http://127.0.0.1:'+str(relay.ports[0])+'/.asb/goose-session-map"; b=json.dumps({"live":"20261104_4","recorded":"20261104_2"}).encode(); r=urllib.request.urlopen(urllib.request.Request(u,data=b,method="POST"),timeout=10); print(r.status)'
  result=subprocess.run(['docker','exec',container,'python3','-c',code],capture_output=True,text=True,timeout=20,check=True)
  assert result.stdout.strip()=='204',result.stdout
  relay.check();print('isolated container -> ASB relay -> host llmtape:',result.stdout.strip())
finally:
  if relay:relay.close()
  if container:subprocess.run(['docker','rm','-f',container],stdout=subprocess.DEVNULL,check=False)
  proxy.send_signal(signal.SIGTERM)
  if proxy.wait(timeout=20)!=0:raise RuntimeError('proxy did not stop cleanly')
  log.close()
progress=json.loads(pathlib.Path(str(cassette)+'.progress.json').read_text())
assert progress['consumed']==0 and progress['rejected']==0,progress
print('control did not consume or reject LLM exchanges:',progress['consumed'],progress['rejected'])
