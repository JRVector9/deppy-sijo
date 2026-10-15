from pathlib import Path
import hashlib,json,gzip,re,subprocess
root=Path('/Users/jr/Desktop/projects/deppy-sijo-performance');out=root/'docs/qa/evidence/2026-10-15-resource-cloud-fixes';sha=lambda b:hashlib.sha256(b).hexdigest();checks={}
m=json.loads((out/'manifest.json').read_text())
for a in m['artifacts']:
 d=(out/a['path']).read_bytes();assert sha(d)==a['stored_sha256'],a['path']
 decoded=gzip.decompress(d) if a['encoding']=='gzip' else d
 assert sha(decoded)==a['source_sha256'],a['path']
checks['artifact_count']=len(m['artifacts'])
s=json.loads((out/'root-final-source-snapshot.json').read_text());b=json.loads((out/'baseline-source-baseline.json').read_text())
for n,h in s['source'].items():assert sha((root/n).read_bytes())==h,n
changed=[n for n,h in b['source'].items() if s['source'].get(n)!=h]
assert set(changed)==set(s['changed_existing']) and len(changed)==10
checks.update({'source_count':len(s['source']),'unchanged_baseline_files':len(b['source'])-len(changed),'changed_existing':len(changed),'added':s['added']})
links=[]
for doc in [root/'docs/qa/2026-10-15-resource-cloud-fixes.md',out/'README.md']:
 text=doc.read_text();assert text.endswith('\n') and not any(line.rstrip()!=line for line in text.splitlines()),doc
 for link in re.findall(r'\]\(([^)]+)\)',text):
  if link.startswith(('http:','https:','#')):continue
  path=re.sub(r':\d+$','',link.split('#')[0]);resolved=doc.parent/path
  if resolved!=out/'verification-result.json':assert resolved.exists(),(doc,link)
  links.append(link)
checks['verified_local_links']=len(links)
for name,cmd in [('diff_check',['git','diff','--check']),('task_patch_reverse_check',['git','apply','--reverse','--check',str(out/'root-final-task-changes.patch')])]:
 r=subprocess.run(cmd,cwd=root,text=True,capture_output=True);checks[name]={'command':cmd,'exit_code':r.returncode,'stdout':r.stdout,'stderr':r.stderr};assert r.returncode==0,checks[name]
gates=json.loads((out/'root-integration-gates-results.json').read_text());assert len(gates)==6 and all(x['exit_code']==0 for x in gates)
counts=json.loads((out/'root-final-test-counts.json').read_text());wk=counts['runs'][-1];assert wk['totals']['failed']==0 and wk['totals']['passed']>5700
checks['workspace_totals']=wk['totals'];checks['workspace_nested_excluded']=wk['nested_results_excluded'];checks['integration_gates_exit0']=[x['name'] for x in gates]
probe=json.loads((out/'root-pty-production-identity.json').read_text());assert probe['compile_exit']==0 and probe['run_exit']==0
for name in ['crates/pty/src/lib.rs','crates/pty/src/workload_signal.rs']:
 assert probe['sources'][str(root/name)]==s['source'][name],name
checks['independent_pty_final_source_match']=True
followup=json.loads((out/'root-pty-followup-green-results.json').read_text());assert len(followup)==2 and all(x['exit_code']==0 for x in followup)
for name in ['pty-natural-exit-red','pty-before-reaper-red']:
 assert json.loads((out/('root-'+name+'-result.json')).read_text())['exit_code']==101
checks['qualified_pty_red_and_green']=True
checks['result']='PASS';(out/'verification-result.json').write_text(json.dumps(checks,indent=2)+'\n');assert (out/'verification-result.json').exists();print(json.dumps(checks,indent=2))
