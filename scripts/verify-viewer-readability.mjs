// Runs the same browser fixture as viewer_readability_chrome.rs without Cargo.
import fs from 'node:fs';
import http from 'node:http';
import os from 'node:os';
import path from 'node:path';
import {spawn} from 'node:child_process';
import {fileURLToPath} from 'node:url';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const read = relative => fs.readFileSync(path.join(root, relative), 'utf8');
const chrome = [process.env.CHROME_PATH, process.env.CHROME_BIN,
  '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome', '/usr/bin/google-chrome',
  '/usr/bin/chromium', '/usr/bin/chromium-browser'].find(file => file && fs.existsSync(file));
if (!chrome) throw Error('Set CHROME_PATH/CHROME_BIN or install Chromium');
const fixture = process.argv.includes('--history') ? 'viewer-history-contract.js'
  : process.argv.includes('--reading') ? 'viewer-reading-contract.js' : 'viewer-readability-contract.js';
const script = read('web/shared/viewer-core.js') + '\n' + read('crates/web-remote/tests/fixtures/' + fixture);
const html = `<!doctype html><meta charset="utf-8"><style>${read('web/shared/viewer-core.css')}</style><body data-status="running"><script type="module">${script.replaceAll('</script', '<\\/script')}</script><script>
new MutationObserver(() => { if (['ok','error'].includes(document.body.dataset.status))
  fetch('/report', {method:'POST',body:document.body.dataset.status+'\\n'+document.body.textContent});
}).observe(document.body,{attributes:true,attributeFilter:['data-status']});</script></body>`;
let resolveReport;
const report = new Promise(resolve => resolveReport = resolve);
const server = http.createServer(async (request, response) => {
  if (request.method === 'POST' && request.url === '/report') {
    const chunks = []; for await (const chunk of request) chunks.push(chunk);
    resolveReport(Buffer.concat(chunks).toString());
    response.writeHead(204); response.end(); return;
  }
  response.writeHead(200, {'Content-Type':'text/html;charset=utf-8'}); response.end(html);
});
await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
const profile = fs.mkdtempSync(path.join(os.tmpdir(), 'deppy-viewer-readability-'));
const child = spawn(chrome, ['--headless=new', '--disable-gpu', '--disable-background-networking',
  '--no-first-run', '--no-default-browser-check', '--user-data-dir='+profile,
  'http://127.0.0.1:'+server.address().port+'/runner.html'], {stdio:['ignore','ignore','pipe']});
let stderr = ''; child.stderr.on('data', chunk => stderr += chunk);
// Register completion before any wait: spawn failures have no exit event, and an
// early browser exit must be remembered when the finally block performs cleanup.
const completed = new Promise(resolve => {
  child.once('error', error => resolve({error}));
  child.once('exit', (code, signal) => resolve({code, signal}));
});
let timer;
try {
  const result = await Promise.race([report,
    new Promise((_, reject) => timer = setTimeout(() => reject(Error('Chromium report timeout\n'+stderr)), 20000)),
    completed.then(({error, code, signal}) => {
      throw error || Error(`Chromium exited before reporting: ${signal || code}\n${stderr}`);
    }),
  ]);
  console.log(result);
  if (!result.startsWith('ok\n')) process.exitCode = 1;
} finally {
  clearTimeout(timer); child.kill('SIGTERM');
  await completed;
  await new Promise(resolve => server.close(resolve));
  fs.rmSync(profile, {recursive:true, force:true});
}
