from pathlib import Path
import json,hashlib,gzip,shutil,re,difflib,subprocess,datetime
root=Path('/Users/jr/Desktop/projects/deppy-sijo-performance')
raw=Path('/private/tmp/deppy-cloud-fix-root-20261015')
base=Path('/private/tmp/deppy-cloud-fix-baseline-20261015')
out=root/'docs/qa/evidence/2026-10-15-resource-cloud-fixes';out.mkdir(parents=True,exist_ok=True)
sha=lambda b:hashlib.sha256(b).hexdigest()
b=json.loads((base/'source-baseline.json').read_text());snapshot={};changed=[];patch=[]
allowed={'crates/app/src/cloud_agent.rs','crates/app/src/cloud_agent/bridge_tests.rs','crates/app/src/cloud_agent/management.rs','crates/app/src/cloud_agent/ui/management_tests.rs','crates/app/src/cloud_agent/ui/permissions.rs','crates/pty/src/lib.rs','crates/pty/src/workload_signal.rs','crates/web-remote/src/ws_api.rs','crates/secret/src/lib.rs','crates/secret/src/redaction.rs'}
for n,old in b['source'].items():
 d=(root/n).read_bytes();snapshot[n]=sha(d)
 if snapshot[n]!=old:
  changed.append(n);patch.extend(difflib.unified_diff((base/'files'/n).read_text().splitlines(True),d.decode().splitlines(True),fromfile='a/'+n,tofile='b/'+n))
assert set(changed)==allowed, changed
new='crates/pty/src/foreground_teardown_tests.rs';d=(root/new).read_bytes();snapshot[new]=sha(d);patch.extend(difflib.unified_diff([],d.decode().splitlines(True),fromfile='/dev/null',tofile='b/'+new))
(raw/'final-task-changes.patch').write_text(''.join(patch))
(raw/'final-source-snapshot.json').write_text(json.dumps({'head':b['head'],'baseline_count':len(b['source']),'final_count':len(snapshot),'changed_existing':changed,'added':[new],'unrelated_preserved':len(b['source'])-len(changed),'missing':[],'source':snapshot},indent=2)+'\n')
def parse(name):
 target=None;results=[];summaries=[]
 def flush():
  if summaries:results.append({'target':target,'result':summaries[-1],'nested_results_excluded':len(summaries)-1})
 for line in (raw/name).read_text().splitlines():
  m=re.match(r'\s+(Running .+|Doc-tests .+)$',line)
  if m:flush();target=m[1];summaries=[]
  m=re.match(r'test result: (ok|FAILED)\. (\d+) passed; (\d+) failed; (\d+) ignored;',line)
  if m:summaries.append({'status':m[1],'passed':int(m[2]),'failed':int(m[3]),'ignored':int(m[4])})
 flush();return {'log':name,'targets':results,'totals':{k:sum(r['result'][k] for r in results) for k in ['passed','failed','ignored']},'nested_results_excluded':sum(r['nested_results_excluded'] for r in results)}
counts=[parse(n) for n in ['secret-pty-green.log','app-cloud-green.log','web-green.log','workspace-green.log']]
(raw/'final-test-counts.json').write_text(json.dumps({'note':'Workspace result stands alone; earlier affected runs are not added to it. Last summary per Cargo harness excludes nested subprocess summaries.','runs':counts},indent=2)+'\n')
manifest=[]
def save(src,name):
 d=src.read_bytes();encoding='identity'
 if len(d)>131072 and ('.log' in name):d=gzip.compress(d,mtime=0);name+='.gz';encoding='gzip'
 dest=out/name;dest.write_bytes(d)
 manifest.append({'path':name,'source':str(src),'source_sha256':sha(src.read_bytes()),'stored_sha256':sha(d),'encoding':encoding,'bytes':len(d)})
for src in sorted(raw.iterdir()):
 if src.is_file() and (src.suffix in {'.py','.json','.log','.rs','.patch'} or '.log.' in src.name or '.json.' in src.name):save(src,'root-'+src.name)
for owner in ['management','pty','transport']:
 p=Path('/private/tmp/deppy-cloud-fix-'+owner+'-results-20261015')
 for src in sorted(p.iterdir()):
  if src.is_file():save(src,owner+'-'+src.name)
for name in ['source-baseline.json','products-before.patch']:save(base/name,'baseline-'+name)
old=root/'docs/reviews/evidence/2026-10-15-resource-cloud-code-review'
for name in ['perf-cloud_answers_optimized.log','root-cloud_answers_optimized-independent.log','perf-identity.json','perf-cloud_answers_optimized.rs','perf-redaction-production.rs','perf-pty_nonfrozen.rs','perf-owned-workload.py']:
 save(old/name,'review-'+name)
(out/'manifest.json').write_text(json.dumps({'utc':datetime.datetime.now(datetime.timezone.utc).isoformat(),'head':b['head'],'artifacts':manifest},indent=2)+'\n')
print(json.dumps({'archived_artifacts':len(manifest),'workspace':counts[-1]['totals'],'nested_excluded':counts[-1]['nested_results_excluded'],'source_files':len(snapshot),'unrelated_preserved':len(b['source'])-len(changed)},indent=2))
