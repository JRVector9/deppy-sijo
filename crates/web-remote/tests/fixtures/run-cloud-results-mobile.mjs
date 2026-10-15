// Isolated browser runner; uses actual production assets and no npm dependencies.
import { readFile, mkdtemp, rm } from 'node:fs/promises';
import { createServer } from 'node:http';
import { spawn } from 'node:child_process';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
const root = resolve(import.meta.dirname, '../../../..');
const read = path => readFile(join(root, path), 'utf8');
const reporter = `<script>new MutationObserver(() => { if (['ok','error'].includes(document.body.dataset.status)) fetch('/report', { method:'POST', body:document.body.dataset.status+'\\n'+document.body.textContent }); }).observe(document.body,{attributes:true,attributeFilter:['data-status']});</script>`;
const html = (await read('crates/web-remote/assets/index.html')).replace('/app.__SHELL_VERSION__.css', '/app.css').replace('  <script type="module" src="/app.__SHELL_VERSION__.js"></script>', `<script src="/fixture.js"></script><script type="module" src="/app.js"></script>${reporter}`);
const assets = new Map([
  ['/', ['text/html', html]],
  ['/fixture.js', ['text/javascript', await read('crates/web-remote/tests/fixtures/cloud-results-mobile.js')]],
  ['/app.js', ['text/javascript', (await read('web/shared/viewer-core.js'))+'\n'+await read('web/shared/cloud-results.js')+'\n'+await read('crates/web-remote/assets/app.js')]],
  ['/app.css', ['text/css', (await read('web/shared/viewer-core.css'))+'\n'+await read('web/shared/mobile-theme.css')+'\n'+await read('web/shared/cloud-results.css')+'\n'+await read('crates/web-remote/assets/app.css')]],
]);
for (const name of ['cloud-results.js','cloud-results.css']) {
  try { assets.set('/'+name, [name.endsWith('.js') ? 'text/javascript' : 'text/css', await read('web/shared/'+name)]); } catch {}
}
let reportResolve; const report = new Promise(resolve => { reportResolve = resolve; });
const server = createServer((req,res) => {
  if (req.url === '/report') { let text = ''; req.on('data', chunk => text += chunk); req.on('end', () => { res.end(); reportResolve(text); }); return; }
  const asset = assets.get(req.url); if (!asset) { res.writeHead(404); res.end(); return; }
  res.setHeader('Content-Type',asset[0]); res.end(asset[1]);
});
await new Promise(resolve => server.listen(0,'127.0.0.1',resolve));
const profile = await mkdtemp(join(tmpdir(), 'deppy-cloud-browser-'));
const chrome = spawn(process.env.CHROME_PATH || '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome', ['--headless=new','--disable-gpu','--disable-background-networking','--disable-component-update','--no-first-run','--no-default-browser-check','--window-size=390,844','--user-data-dir='+profile,'http://127.0.0.1:'+server.address().port], { stdio:'ignore' });
const timer = setTimeout(() => reportResolve('error\nbrowser fixture timed out'), 10000);
try { const result = await report; console.log(result.match(/CLOUD_RESULTS_MOBILE_(?:OK|ERROR):[\s\S]*/)?.[0] || result); process.exitCode = result.startsWith('ok\n') ? 0 : 1; }
finally { clearTimeout(timer); chrome.kill(); await new Promise(resolve => chrome.once('exit', resolve)); server.close(); await rm(profile,{recursive:true,force:true,maxRetries:10,retryDelay:100}); }
