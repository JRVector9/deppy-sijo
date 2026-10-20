from pathlib import Path
import hashlib, json, plistlib, shutil, subprocess, tomllib

root=Path('/Users/jr/Desktop/projects/deppy-sijo-command-center-20261020')
version=tomllib.loads((root/'Cargo.toml').read_text())['workspace']['package']['version']
assert version=='0.14.0'
bundle=root/'target'/('bundle-'+version)/'Deppy Sijo.app'
assert not bundle.exists(), 'Preserve any existing artifact; inspect before replacing'
mac=bundle/'Contents/MacOS'
resources=bundle/'Contents/Resources'
mac.mkdir(parents=True)
resources.mkdir()
identity='Developer ID Application: VectorNine INC (ZDTU5LS35K)'
reported=subprocess.check_output([str(root/'target/debug/deppy-sijo'),'--version'],text=True).strip()
assert reported=='Deppy Sijo '+version, reported
for name in ['deppy-sijo','deppy-mcp-proxy','deppy-cloudflared']:
    shutil.copy2(root/'target/debug'/name,mac/name)
shutil.copy2(root/'scripts/vendor/cloudflared-LICENSE',resources/'cloudflared-LICENSE')
plist={
    'CFBundlePackageType':'APPL','CFBundleIdentifier':'app.vector9.deppy-sijo',
    'CFBundleName':'Deppy Sijo','CFBundleDisplayName':'Deppy Sijo',
    'CFBundleExecutable':'deppy-sijo','CFBundleVersion':version,
    'CFBundleShortVersionString':version,'LSMinimumSystemVersion':'11.0',
    'NSHighResolutionCapable':True,'NSSupportsAutomaticGraphicsSwitching':True,
}
(bundle/'Contents/Info.plist').write_bytes(plistlib.dumps(plist))
for name in ['deppy-cloudflared','deppy-mcp-proxy','deppy-sijo']:
    suffix={ 'deppy-sijo':'', 'deppy-mcp-proxy':'.mcp-proxy', 'deppy-cloudflared':'.tunnel'}[name]
    subprocess.run(['codesign','--force','--sign',identity,'--identifier','app.vector9.deppy-sijo'+suffix,str(mac/name)],check=True)
subprocess.run(['codesign','--force','--sign',identity,str(bundle)],check=True)
actual=subprocess.check_output([str(mac/'deppy-sijo'),'--version'],text=True).strip()
assert actual==reported
info=plistlib.loads((bundle/'Contents/Info.plist').read_bytes())
assert info['CFBundleVersion']==info['CFBundleShortVersionString']==version
source=subprocess.check_output(['git','rev-parse','HEAD'],cwd=root,text=True).strip()
diff=subprocess.check_output(['git','diff','--','Cargo.toml','Cargo.lock'],cwd=root)
result={
 'kind':'local signed development build, debug profile; not notarized production distribution',
 'branch':'feat/agent-command-center-v2-20261020','source_commit':source,
 'version_change':{'source_old':'0.13.0','last_shipped':'0.13.1','new':version},
 'source_delta':'Cargo.toml/Cargo.lock workspace version only',
 'version_diff_sha256':hashlib.sha256(diff).hexdigest(),
 'reported_version':actual,'plist_versions':{k:info[k] for k in ['CFBundleVersion','CFBundleShortVersionString']},
 'bundle':str(bundle),'opt_in':'DEPPY_COMMAND_CENTER_V2=1',
 'binary_sha256':{p.name:hashlib.sha256(p.read_bytes()).hexdigest() for p in mac.iterdir()},
}
(bundle.parent/'build-result.json').write_text(json.dumps(result,indent=2)+'\n')
print(json.dumps(result,indent=2))
