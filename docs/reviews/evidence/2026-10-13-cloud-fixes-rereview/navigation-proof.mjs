import { readFile, writeFile, mkdtemp, rm } from 'node:fs/promises';
import { createServer } from 'node:http';
import { spawn, execFileSync } from 'node:child_process';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
const root = '/Users/jr/Desktop/projects/deppy-sijo-performance';
const revision = process.argv[2] || 'current';
const read = path => revision === 'current' ? readFile(join(root, path), 'utf8') : Promise.resolve(execFileSync('git', ['show', revision + ':' + path], {cwd:root, encoding:'utf8'}));
const prelude = (await read('crates/web-remote/tests/fixtures/cloud-results-mobile.js')).split('\n\n(async () =>')[0];
const tests = `
(async () => {
  const sleep = ms => new Promise(resolve => setTimeout(resolve,ms));
  const stages = [];
  const readState = name => {
    const feedback = document.querySelector('.cloud-feedback');
    const notice = document.getElementById('notice-banner');
    stages.push({ name, feedback:feedback?.textContent, notice:notice?.textContent, noticeHidden:notice?.hidden,
      resultsVisible:!document.getElementById('cloud-results').hidden,
      watches:cloudSent.filter(frame => frame.type === 'watch').map(frame => frame.session),
      failureShown: /실패|못했습니다|없어.*취소|종료되었습니다/.test((feedback?.textContent||'')+' '+(notice?.hidden?'':notice?.textContent||'')) });
  };
  const dashboard = sessions => cloudSocket.message({type:'dashboard',workspaces:[
    {id:'ws-original',name:'Original',state:'active',sessions},
    {id:'ws-other',name:'Other',state:'warm',sessions:[]}
  ]});
  try {
    for(let n=0;n<100 && !document.querySelector('[data-cloud-run="run-a"]');n++) await sleep(20);
    document.getElementById('cloud-results-toggle').click();
    dashboard([]);
    document.querySelector('[data-cloud-run="run-a"] [data-cloud-action="navigate"]').click();
    await sleep(25);
    readState('queued_for_absent_original');
    dashboard([{id:'session-original',selection_id:'session-original',title:'Closed original',exited:true}]);
    await sleep(20);
    readState('closed_original_dashboard');
    cloudSocket.snapshot();
    readState('snapshot_after_closed');
    await sleep(6100);
    readState('closed_notice_after_6_seconds');
    dashboard([]);
    document.querySelector('[data-cloud-run="run-a"] [data-cloud-action="navigate"]').click();
    await sleep(25);
    readState('second_queued_for_absent_original');
    const realNow = Date.now;
    Date.now = () => realNow() + 31000;
    dashboard([]);
    await sleep(20);
    readState('timeout_consumed_after_31_seconds');
    cloudSocket.snapshot();
    readState('snapshot_after_timeout');
    Date.now = realNow;
    const result = {revision: ${JSON.stringify(revision)}, stages, errors:cloudErrors};
    document.body.append('NAVIGATION_REREVIEW_REPORT:'+JSON.stringify(result));
    document.body.dataset.status = stages.every(stage => !stage.failureShown) && cloudErrors.length===0 ? 'reproduced' : 'unexpected';
  } catch(error) { document.body.append('NAVIGATION_REREVIEW_ERROR:'+error.stack); document.body.dataset.status='error'; }
})();`;
const reporter = `<script>new MutationObserver(() => { if(['reproduced','unexpected','error'].includes(document.body.dataset.status)) fetch('/report',{method:'POST',body:document.body.dataset.status+'\\n'+document.body.textContent}); }).observe(document.body,{attributes:true,attributeFilter:['data-status']});</script>`;
const html = (await read('crates/web-remote/assets/index.html')).replace('/app.__SHELL_VERSION__.css','/app.css').replace('  <script type="module" src="/app.__SHELL_VERSION__.js"></script>', `<script src="/fixture.js"></script><script type="module" src="/app.js"></script>${reporter}`);
const assets = new Map([
  ['/', ['text/html',html]],
  ['/fixture.js',['text/javascript',prelude+'\n'+tests]],
  ['/app.js',['text/javascript',(await read('web/shared/viewer-core.js'))+'\n'+await read('web/shared/cloud-results.js')+'\n'+await read('crates/web-remote/assets/app.js')]],
  ['/app.css',['text/css',(await read('web/shared/viewer-core.css'))+'\n'+await read('web/shared/mobile-theme.css')+'\n'+await read('web/shared/cloud-results.css')+'\n'+await read('crates/web-remote/assets/app.css')]],
]);
let reportResolve; const report = new Promise(resolve=>reportResolve=resolve);
const server = createServer((req,res)=>{
  if(req.url==='/report'){let body='';req.on('data',chunk=>body+=chunk);req.on('end',()=>{res.end();reportResolve(body);});return;}
  const asset=assets.get(req.url);if(!asset){res.writeHead(404);res.end();return;}
  res.setHeader('Content-Type',asset[0]);res.end(asset[1]);
});
await new Promise(resolve=>server.listen(0,'127.0.0.1',resolve));
const profile=await mkdtemp(join(tmpdir(),'deppy-nav-rereview-'));
const chrome=spawn('/Applications/Google Chrome.app/Contents/MacOS/Google Chrome',['--headless=new','--disable-gpu','--disable-background-networking','--disable-component-update','--no-first-run','--no-default-browser-check','--window-size=390,844','--user-data-dir='+profile,'http://127.0.0.1:'+server.address().port],{stdio:'ignore'});
const timer=setTimeout(()=>reportResolve('error\nfixture timed out'),15000);
try {const output=await report; const summary=output.match(/NAVIGATION_REREVIEW_(?:REPORT|ERROR):[\s\S]*/)?.[0]||output; console.log(summary);await writeFile('/private/tmp/deppy-cloud-navigation-rereview-'+revision+'-20261013.log',summary+'\n'); process.exitCode=output.startsWith('reproduced\n')?0:1;}
finally {clearTimeout(timer);chrome.kill();await new Promise(resolve=>chrome.once('exit',resolve));server.close();await rm(profile,{recursive:true,force:true,maxRetries:10,retryDelay:100});}
