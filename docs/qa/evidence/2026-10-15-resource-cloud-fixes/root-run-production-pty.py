from pathlib import Path
import json, subprocess, hashlib, shutil
raw=Path('/private/tmp/deppy-cloud-fix-root-20261015')
root=Path('/Users/jr/Desktop/projects/deppy-sijo-performance')
identity_path=raw/'pty-production-identity.json'
old=json.loads(identity_path.read_text())
args=list(old['compile_command'])
old_rlib=Path(next(a.removeprefix('pty=') for a in args if a.startswith('pty=')))
rlib=max((root/'target/debug/deps').glob('libpty-*.rlib'),key=lambda p:p.stat().st_mtime)
args[args.index('pty='+str(old_rlib))]='pty='+str(rlib)
dependency_file=rlib.with_name(rlib.name.removeprefix('lib')).with_suffix('.d')
dependency_text=dependency_file.read_text()
assert str(root/'target/debug/deps') in dependency_text and 'crates/pty/src/lib.rs' in dependency_text, 'Production dependency cohort must come from canonical source'
assert rlib.stat().st_mtime > max((root/n).stat().st_mtime for n in ['crates/pty/src/lib.rs','crates/pty/src/workload_signal.rs']), 'Compile canonical production rlib first'
for n in ['pty-production-compile.log','pty-production-green.log','pty-production-identity.json']:
    if not (raw/(n+'.attempt1')).exists():
        shutil.copyfile(raw/n,raw/(n+'.attempt1'))
source_paths=[str(rlib) if p==str(old_rlib) else p for p in old['sources']]
source_paths.append(str(dependency_file))
sources={p:hashlib.sha256(Path(p).read_bytes()).hexdigest() for p in source_paths}
with (raw/'pty-production-compile.log').open('w') as f:
    built=subprocess.run(args,cwd=root,stdout=f,stderr=subprocess.STDOUT)
assert built.returncode==0
binary=Path(args[args.index('-o')+1])
with (raw/'pty-production-green.log').open('w') as f:
    ran=subprocess.run([str(binary)],cwd=root,stdout=f,stderr=subprocess.STDOUT)
record={'compile_command':args,'compile_exit':built.returncode,'sources':sources,'pty_rlib_mtime':rlib.stat().st_mtime,'run_exit':ran.returncode,'run_command':[str(binary)],'binary_sha256':hashlib.sha256(binary.read_bytes()).hexdigest()}
identity_path.write_text(json.dumps(record,indent=2)+'\n')
print(json.dumps(record,indent=2))
raise SystemExit(ran.returncode)
