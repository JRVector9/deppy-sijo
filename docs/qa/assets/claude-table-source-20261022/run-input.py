from pathlib import Path
import os,subprocess,sys,shutil,hashlib,json
root=Path(__file__).parent
mode=sys.argv[1]
version=sys.argv[2] if len(sys.argv)>2 else '2.1.305'
case=root/f'{mode}-input-{version.replace(chr(46),chr(45))}'
case.mkdir(exist_ok=True)
profile=case/'profile';project=case/'project';project.mkdir(exist_ok=True)
if not profile.exists():
 shutil.copytree(root/'claude/profile',profile)
 old=str(root/'claude/project');new=str(project)
 for p in profile.rglob('*'):
  if p.is_file() and p.suffix in ('.json','.jsonl'):
   try:
    content=p.read_text().replace(old,new)
    if p.suffix=='.jsonl':
     records=[json.loads(line) for line in content.splitlines()]
     content=''.join(json.dumps(j)+'\n' for j in records if j.get('type') in ('user','assistant'))
    p.write_text(content)
   except UnicodeDecodeError:pass
 for p in (profile/'projects').iterdir():
  if p.is_dir():p.rename(profile/'projects'/new.replace('/','-'))
base={k:os.environ[k] for k in ['PATH','LANG','LC_ALL','USER','LOGNAME','HOME','TMPDIR','SHELL'] if k in os.environ}
base.update({'BROWSER':'/usr/bin/false','CLAUDE_CONFIG_DIR':str(profile),'CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC':'1','CLAUDE_CODE_DISABLE_AUTO_UPDATE':'1','ANTHROPIC_API_KEY':'offline-fixture-not-a-real-key','ANTHROPIC_BASE_URL':'http://127.0.0.1:9','CLAUDE_CODE_NO_FLICKER':'1' if mode=='fullscreen' else '0'})
binary=Path('/Users/jr/.local/share/claude/versions')/version
sid=(root/'claude/session-id').read_text().strip()
args=['-f',str(root/'offline.sb'),str(binary),'--bare','--permission-mode','default','--setting-sources','','--strict-mcp-config','--mcp-config','{"mcpServers":{}}','--resume',sid,'--settings',json.dumps({'tui':'fullscreen' if mode=='fullscreen' else 'default'})]
base.update({'DEPPY_TABLE_PROBE_BOOTSTRAP':'1','DEPPY_TABLE_PROBE_SCROLL_TOP':'1','DEPPY_TABLE_PROBE_PROGRAM':'/usr/bin/sandbox-exec','DEPPY_TABLE_PROBE_ARGS':'\n'.join(args),'DEPPY_TABLE_PROBE_ROOT':str(case),'DEPPY_TABLE_PROBE_CWD':str(project)})
(case/'provenance.json').write_text(json.dumps({'binary':str(binary),'sha256':hashlib.file_digest(binary.open('rb'),'sha256').hexdigest(),'args':args,'mode':mode,'version':version,'fixture_only':True},indent=2)+'\n')
with (case/'probe.log').open('w') as log:
 proc=subprocess.run([str(root/'probe-input'),'--exact','installed_agent_table_resize_probe','--nocapture'],env=base,stdout=log,stderr=subprocess.STDOUT,timeout=40)
print(case, 'exit',proc.returncode)
print((case/'probe.log').read_text()[-1600:])
print((case/'bootstrap-status.txt').read_text())
raise SystemExit(proc.returncode)
