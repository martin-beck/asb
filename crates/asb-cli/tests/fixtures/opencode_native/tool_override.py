import json,os,pathlib,subprocess,threading,tempfile
from http.server import BaseHTTPRequestHandler,ThreadingHTTPServer
# Optional installed-client probe. Keep temporary data in the project build tree.
root=pathlib.Path(tempfile.mkdtemp(prefix='opencode-native-probe-', dir=os.environ['ASB_OPENCODE_PROBE_DIR']))
home=root/'home';home.mkdir(exist_ok=True)
work=root/'work';work.mkdir(exist_ok=True)
plugin=root/'plugin.mjs'
plugin.write_text('export default async () => ({tool: {bash: {description:"fixture", args: {command: {type:"string"}, timeout: {type:"integer"}}, execute: async (args) => { await Bun.write('+json.dumps(str(root/'called.json'))+', JSON.stringify(args)); return "intercepted" }}}, "tool.definition": async (input, output) => { if (input.toolID === "bash") output.jsonSchema = {type:"object",properties:{command:{type:"string"},timeout:{type:"integer"}},required:["command"]} }});\n')
class Handler(BaseHTTPRequestHandler):
 calls=0
 def log_message(self,*args):pass
 def do_POST(self):
  n=int(self.headers.get('Content-Length','0'));data=json.loads(self.rfile.read(n)); Handler.calls+=1
  (root/('request-%s.json'%Handler.calls)).write_text(json.dumps(data))
  if data.get('tools') and not any(m.get('role') == 'tool' for m in data.get('messages',[])):
   message={'role':'assistant','content':None,'tool_calls':[{'id':'call_1','type':'function','function':{'name':'bash','arguments':json.dumps({'command':'touch SHOULD_NOT_EXIST','timeout':1234})}}]}; finish='tool_calls'
  else:message={'role':'assistant','content':'done'};finish='stop'
  response={'id':'fixture','object':'chat.completion','created':1,'model':'fixture','choices':[{'index':0,'message':message,'finish_reason':finish}],'usage':{'prompt_tokens':1,'completion_tokens':1,'total_tokens':2}}
  delta={k:v for k,v in message.items() if k!='role'}; delta['role']='assistant'
  event={'id':'fixture','object':'chat.completion.chunk','created':1,'model':'fixture','choices':[{'index':0,'delta':delta,'finish_reason':finish}]}
  b=('data: '+json.dumps(event)+'\n\ndata: [DONE]\n\n').encode();self.send_response(200);self.send_header('Content-Type','text/event-stream');self.send_header('Content-Length',str(len(b)));self.end_headers();self.wfile.write(b)
server=ThreadingHTTPServer(('127.0.0.1',0),Handler);threading.Thread(target=server.serve_forever,daemon=True).start()
config={'$schema':'https://opencode.ai/config.json','plugin':[str(plugin)],'provider':{'asb':{'npm':'@ai-sdk/openai-compatible','name':'fixture','options':{'baseURL':f'http://127.0.0.1:{server.server_port}/v1','apiKey':'local'},'models':{'fixture':{'name':'fixture'}}}}}
env=dict(os.environ,HOME=str(home),XDG_CONFIG_HOME=str(home/'config'),XDG_DATA_HOME=str(home/'data'),XDG_CACHE_HOME=str(home/'cache'),OPENCODE_CONFIG_CONTENT=json.dumps(config),NO_PROXY='*',OTEL_SDK_DISABLED='true')
for k in ['HTTP_PROXY','HTTPS_PROXY','ALL_PROXY','http_proxy','https_proxy','all_proxy']:env.pop(k,None)
try:
 p=subprocess.run([os.environ['ASB_OPENCODE_BINARY'],'run','--auto','--dir',str(work),'--model','asb/fixture','--format','json','Use bash to execute the command.'],env=env,capture_output=True,text=True,timeout=90)
 (root/'stdout.log').write_text(p.stdout);(root/'stderr.log').write_text(p.stderr)
 result={'exit':p.returncode,'provider_calls':Handler.calls,'plugin_called':(root/'called.json').exists(),'original_executed':(work/'SHOULD_NOT_EXIST').exists()}
 print(json.dumps(result))
 assert p.returncode == 0 and result['plugin_called'] and not result['original_executed'], f'probe failed; inspect {root}'
 assert json.loads((root/'called.json').read_text())['timeout'] == 1234
 assert any(tool.get('function',{}).get('parameters',{}).get('required') == ['command'] for request in root.glob('request-*.json') for tool in json.loads(request.read_text()).get('tools',[]) if tool.get('function',{}).get('name') == 'bash'), f'provider schema changed; inspect {root}'
 assert any(m.get('role')=='tool' and m.get('content')=='intercepted' for request in root.glob('request-*.json') for m in json.loads(request.read_text()).get('messages', [])), f'provider did not receive replayed result; inspect {root}'
finally:server.shutdown();server.server_close()
