import json,os,pathlib,subprocess,tempfile,threading
from http.server import BaseHTTPRequestHandler,ThreadingHTTPServer
root=pathlib.Path(tempfile.mkdtemp(prefix='workbuddy-client-',dir=os.environ['ASB_WORKBUDDY_PROBE_DIR']))
home=root/'home'; home.mkdir();work=root/'work';work.mkdir();config=home/'config';config.mkdir()
class Handler(BaseHTTPRequestHandler):
 calls=0
 def log_message(self,*args):pass
 def do_POST(self):
  data=json.loads(self.rfile.read(int(self.headers.get('Content-Length','0'))));Handler.calls+=1
  tools=data.get('tools',[]);names=[x.get('function',{}).get('name','') for x in tools];name=next((n for n in names if n.lower()=='bash'),None)
  has_result=any(x.get('role')=='tool' for x in data.get('messages',[]))
  if name and not has_result:
   msg={'role':'assistant','content':None,'tool_calls':[{'id':'call_fixture','type':'function','function':{'name':name,'arguments':json.dumps({'command':'printf workbuddy_native > evidence.txt; cat evidence.txt'})}}]};finish='tool_calls'
  else:msg={'role':'assistant','content':'done'};finish='stop'
  if data.get('stream'):
   event={'id':'fixture','object':'chat.completion.chunk','created':1,'model':'fixture','choices':[{'index':0,'delta':msg,'finish_reason':finish}]};b=('data: '+json.dumps(event)+'\n\ndata: [DONE]\n\n').encode();ctype='text/event-stream'
  else:
   event={'id':'fixture','object':'chat.completion','created':1,'model':'fixture','choices':[{'index':0,'message':msg,'finish_reason':finish}],'usage':{'prompt_tokens':1,'completion_tokens':1,'total_tokens':2}};b=json.dumps(event).encode();ctype='application/json'
  self.send_response(200);self.send_header('Content-Type',ctype);self.send_header('Content-Length',str(len(b)));self.end_headers();self.wfile.write(b)
server=ThreadingHTTPServer(('127.0.0.1',0),Handler);threading.Thread(target=server.serve_forever,daemon=True).start();url=f'http://127.0.0.1:{server.server_port}/v1'
(config/'models.json').write_text(json.dumps([{'id':'fixture','name':'fixture','url':url,'api':'openai'}]))
env=dict(os.environ,HOME=str(home),CODEBUDDY_CONFIG_DIR=str(config),CODEBUDDY_API_KEY='fixture',CODEBUDDY_AUTH_TOKEN='fixture',CODEBUDDY_BASE_URL=url,CODEBUDDY_MODEL='fixture',CODEBUDDY_API_KEY_HELPER_DISABLED='1',CODEBUDDY_IS_SANDBOX='1',NO_PROXY='*',OTEL_SDK_DISABLED='true')
for k in ('HTTP_PROXY','HTTPS_PROXY','ALL_PROXY','http_proxy','https_proxy','all_proxy'):env.pop(k,None)
try:
 p=subprocess.run([os.environ['ASB_WORKBUDDY_BINARY'],'--print','--output-format','json','--model','fixture','--no-session-persistence','-y','Use bash.'],cwd=work,env=env,capture_output=True,text=True,timeout=90)
 (root/'stdout.log').write_text(p.stdout);(root/'stderr.log').write_text(p.stderr)
 print(json.dumps({'exit':p.returncode,'provider_calls':Handler.calls,'root':str(root),'command_executed':(work/'evidence.txt').exists()}))
finally:server.shutdown();server.server_close()
