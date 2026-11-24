"""Opt-in installed randomness readback on pages_fixture.m; not a consumer comparison.

Run through android-webview-clock/run_ios.py with --page-registry. Set
FRONTCAP_WEBVIEW_EXCHANGE to the built webview_exchange example. The owned runner
handles app installation and retirement; this controller only creates fixture pages
and issues document-bound reads/advances.
"""
import argparse,json,os,subprocess,time
from pathlib import Path
p=argparse.ArgumentParser();p.add_argument('--device',required=True);p.add_argument('--bundle-id',required=True);p.add_argument('--output',type=Path,required=True);p.add_argument('--page-registry',action='store_true');a=p.parse_args()
root=Path(subprocess.check_output(['xcrun','simctl','get_app_container',a.device,a.bundle_id,'data'],text=True).strip())/'Documents'
exe=Path(os.environ['FRONTCAP_WEBVIEW_EXCHANGE']).resolve(strict=True)
def load(p):return json.loads(p.read_text()) if p.exists() else None
def wait(f):
 end=time.monotonic()+40
 while time.monotonic()<end:
  value=f()
  if value:return value
  time.sleep(.1)
 raise TimeoutError('readback condition not reached')
indices={};samples=[]
def call(page,op='read',delta=0,document=None):
 e=page['endpoint'];g=e['generation'];indices[g]=indices.get(g,0)+1
 request=dict(generation=g,request_id=indices[g],operation=op,delta_ns=delta)
 out=subprocess.run([str(exe)],input=json.dumps(dict(endpoint={k:e[k] for k in ('generation','pid','port')},request=request,document=document)),capture_output=True,text=True,timeout=15)
 if out.returncode:raise RuntimeError(out.stderr)
 return json.loads(out.stdout)
for n in (1,2):
 (root/'page-command.json').write_text(json.dumps(dict(sequence=n,action='create',name=f'child{n}')))
 wait(lambda:load(root/'page-command-result.json')=={'sequence':n})
 page=wait(lambda:next((x for x in (load(root/'webview-pages.json') or {}).get('pages',[]) if x['id'] not in {s['page'] for s in samples}),None))
 def ready():
  r=call(page)
  if r['status']!='ok':return None
  q=r.get('starting_randomness')
  return r if q and q['crypto_uuid_calls']>=1 else None
 before=wait(ready);doc=before['document'];q=before['starting_randomness']
 assert q==dict(document=doc,seed=42,math_calls=0,crypto_byte_calls=1,crypto_uuid_calls=1,observation_errors=0,uncontrolled_calls=0),q
 held=call(page,document=doc);assert held['starting_randomness']==q and held['state']==before['state']
 after=call(page,'advance',100_000_000,doc);r=after['starting_randomness'];assert r==dict(q,math_calls=10),r
 again=call(page,document=doc);assert again['starting_randomness']==r and again['state']==after['state']
 samples.append(dict(page=page['id'],document=doc,before=q,after=r))
 print('child',n,'readback passed',flush=True)
assert samples[0]['document']!=samples[1]['document']
result=dict(passed=True,scope='installed WKWebView fixture plus typed Rust exchange',samples=samples)
(a.output/'frame.json').write_text(json.dumps(result,indent=2))
