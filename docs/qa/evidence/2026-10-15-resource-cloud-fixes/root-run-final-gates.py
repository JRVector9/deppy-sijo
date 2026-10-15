from pathlib import Path
import os, subprocess, json, time
root = Path('/Users/jr/Desktop/projects/deppy-sijo-performance')
raw = Path('/private/tmp/deppy-cloud-fix-root-20261015')
env = dict(os.environ, CARGO_BUILD_JOBS='2')
commands = [
 ('app-cloud-green.log', ['cargo','test','--offline','--locked','-p','deppy-sijo','--bin','deppy-sijo','cloud_agent::','--','--test-threads=1']),
 ('workspace-green.log', ['cargo','test','--offline','--locked','--workspace','--no-fail-fast','--','--test-threads=1']),
 ('clippy-green.log', ['cargo','clippy','--offline','--locked','--workspace','--all-targets','--','-D','warnings']),
 ('fmt-green.log', ['cargo','fmt','--all','--','--check']),
 ('boundary-green.log', ['cargo','run','--offline','--locked','-p','xtask','--','check-boundary']),
 ('deps-green.log', ['cargo','run','--offline','--locked','-p','xtask','--','check-deps']),
]
results=[]
for name, args in commands:
    start=time.time()
    with (raw/name).open('w') as f:
        run=subprocess.run(args, cwd=root, env=env, stdout=f, stderr=subprocess.STDOUT)
    record={'name':name.removesuffix('-green.log'),'command':args,'exit_code':run.returncode,'seconds':time.time()-start,'log':str(raw/name)}
    results.append(record)
    (raw/'integration-gates-results.json').write_text(json.dumps(results,indent=2)+'\n')
    print(json.dumps(record),flush=True)
    if run.returncode:
        raise SystemExit(run.returncode)
