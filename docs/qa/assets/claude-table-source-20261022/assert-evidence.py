from pathlib import Path
import hashlib,json
root=Path(__file__).parent
manifest=json.loads((root/'manifest.json').read_text())
for item in manifest['files']:
 p=root/item['path']
 assert p.stat().st_size==item['bytes'],p
 assert hashlib.sha256(p.read_bytes()).hexdigest()==item['sha256'],p
results=json.loads((root/'measurements.json').read_text())
assert len(results)==24
for item in results:
 stage,cols=item['stage'],item['cols'];case=root/item['case']
 name=f'application-top-{stage}-{cols}.txt' if case.name.startswith('fullscreen') else f'history-{stage}-{cols}.txt'
 text=(case/name).read_text()
 borders=[len(line.strip()) for line in text.splitlines() if (line.strip().startswith('┌') and line.strip().endswith('┐')) or (line.strip().startswith('└') and line.strip().endswith('┘'))]
 assert borders==item['complete_table_borders'],(case,name,borders)
 assert 'Make auto mode your default permission' not in text,(case,name)
 assert 'auto mode on' not in text,(case,name)
 if cols!=42:
  expected=cols-4 if case.name.startswith('fullscreen') else 76
  assert borders==[expected,expected],(case,name,borders)
 elif case.name.startswith('fullscreen'):
  assert all(marker in text for marker in ('Agent: ROW_ALPHA','Status: ready','Agent: ROW_OMEGA','Status: done')),(case,name)
 permissions=json.loads((case/'permission-state.json').read_text())['permission_records']
 assert permissions and all(record['permissionMode']=='default' for record in permissions),case
 assert 'dismissed=true' in (case/'bootstrap-status.txt').read_text(),case
print(f'PASS: {len(results)} observed stages; all evidence hashes, complete borders, narrow cards and clean modal state verified.')
