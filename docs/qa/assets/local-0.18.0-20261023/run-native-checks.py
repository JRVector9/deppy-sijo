"""Serialized native checks/build only. Does not launch the desktop app."""
from pathlib import Path
import hashlib,json,os,subprocess,time
ROOT=Path(__file__).resolve().parents[4]
EV=Path(__file__).resolve().parent
env=os.environ.copy()
env.update(CARGO_BUILD_JOBS='2',CARGO_TARGET_DIR='/Users/jr/Desktop/projects/deppy-sijo-ssh-workspace-20261022/target')
commands=[
 ('launcher-final',['cargo','test','--offline','--locked','-p','deppy-sijo','--bin','deppy-sijo','ui::agent_launcher::tests','--','--test-threads=4']),
 ('app-final',['cargo','test','--offline','--locked','-p','deppy-sijo','--','--test-threads=4']),
 ('clippy-final',['cargo','clippy','--offline','--locked','-p','deppy-sijo','-p','runtime','-p','terminal','-p','deppy-remote-host','-p','ssh-transport','--all-targets','--','-D','warnings']),
 ('fmt-final',['cargo','fmt','--all','--','--check']),
 ('boundary-final',['cargo','run','--offline','--locked','-p','xtask','--','check-boundary']),
 ('native-build',['cargo','build','--offline','--locked','-p','deppy-sijo','-p','mcp-proxy','-p','ssh-askpass']),
]
for name,cmd in commands:
 start=time.monotonic()
 with (EV/(name+'.log')).open('w') as log:
  result=subprocess.run(cmd,cwd=ROOT,env=env,stdout=log,stderr=subprocess.STDOUT)
 record={'command':cmd,'exit_code':result.returncode,'elapsed_seconds':round(time.monotonic()-start,3)}
 (EV/(name+'-result.json')).write_text(json.dumps(record,indent=2)+'\n')
 print(name+': '+json.dumps(record),flush=True)
 print('\n'.join((EV/(name+'.log')).read_text().splitlines()[-8:]),flush=True)
 if result.returncode: raise SystemExit(result.returncode)
proof=json.loads((EV/'source-proof.json').read_text())
for relative,digest in proof['files'].items():
 assert hashlib.sha256((ROOT/relative).read_bytes()).hexdigest()==digest,'Source changed: '+relative
print('All serialized native checks/build PASS; source proof unchanged.',flush=True)
