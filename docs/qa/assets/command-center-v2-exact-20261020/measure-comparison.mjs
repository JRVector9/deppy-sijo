// Isolated local Chrome evidence; never attaches to Deppy or a user's browser.
import fs from 'node:fs';
import path from 'node:path';
import {spawn} from 'node:child_process';
const [root,origin,destination,source='working-tree']=process.argv.slice(2);
if(!root||!origin||!destination)throw Error('Usage: node measure-comparison.mjs REPO LOCAL_FIXTURE_ORIGIN OUTPUT SOURCE');
if(!/^http:\/\/127\.0\.0\.1:\d+$/.test(origin))throw Error('Local fixture only');
fs.mkdirSync(destination,{recursive:true});
const profile=fs.mkdtempSync('/private/tmp/deppy-cc-exact-proof-');
const chrome=spawn('/Applications/Google Chrome.app/Contents/MacOS/Google Chrome',['--headless=new','--disable-gpu','--disable-background-networking','--no-first-run','--no-default-browser-check','--remote-debugging-port=0','--user-data-dir='+profile,'about:blank'],{stdio:'ignore'});
const delay=ms=>new Promise(r=>setTimeout(r,ms));let ws;
try{
 for(let i=0;i<100&&!fs.existsSync(profile+'/DevToolsActivePort');i++)await delay(100);
 const port=fs.readFileSync(profile+'/DevToolsActivePort','utf8').split('\n')[0];
 const tabs=await(await fetch('http://127.0.0.1:'+port+'/json')).json();
 ws=new WebSocket(tabs.find(t=>t.type==='page').webSocketDebuggerUrl);await new Promise(r=>ws.addEventListener('open',r,{once:true}));
 const pending=new Map();let serial=0;
 ws.addEventListener('message',e=>{const m=JSON.parse(e.data);if(!m.id)return;const p=pending.get(m.id);pending.delete(m.id);m.error?p.reject(Error(JSON.stringify(m.error))):p.resolve(m.result);});
 const cdp=(method,params={})=>new Promise((resolve,reject)=>{const id=++serial;pending.set(id,{resolve,reject});ws.send(JSON.stringify({id,method,params}));});
 const run=async expression=>{const r=await cdp('Runtime.evaluate',{expression,returnByValue:true,awaitPromise:true});if(r.exceptionDetails)throw Error(JSON.stringify(r.exceptionDetails));return r.result.value;};
 await cdp('Page.enable');await cdp('Emulation.setFocusEmulationEnabled',{enabled:true});
 const snapshot=JSON.parse(fs.readFileSync(path.join(root,'docs/qa/assets/command-center-v2-fixes-20261020/pwa/matched-snapshot.json'),'utf8'));
 for(const p of snapshot.projects)p.available_agents=[{provider:'Codex',model:'GPT-6.1-Sol',effort:'xhigh'},{provider:'Claude',model:'Opus 5.5',effort:'xhigh'}];
 const results={source,kind:'Actual approved HTML and shared renderer, matching snapshot; isolated Chrome, not native app or physical phone',cases:[]};
 for(const width of [1440,1200,1000,760,390])for(const light of [false,true])for(const kind of ['approved','shared']){
  const height=width===390?844:900;
  await cdp('Emulation.setDeviceMetricsOverride',{width,height,deviceScaleFactor:1,mobile:false});
  await cdp('Page.navigate',{url:origin+(kind==='approved'?'/reference':'/?visual=1')});
  let ready=false;for(let i=0;i<120;i++){await delay(25);ready=await run(kind==='approved'?'!!document.getElementById("prompt")':'!!globalThis.ccFixture');if(ready)break;}if(!ready)throw Error('fixture not ready');
  if(kind==='approved')await run(`document.querySelector('.preview').remove();document.body.style.cssText='margin:0;padding:0';document.getElementById('app').style.cssText='width:100vw;height:100dvh;max-width:none;margin:0;border:0';document.documentElement.classList.toggle('light',${light});document.body.classList.toggle('light',${light});document.getElementById('app').classList.toggle('light',${light});true`);
  else await run(`ccFixture.receive(${JSON.stringify({...snapshot,dark_mode:!light})});true`);
  await delay(90);
  const measure=async focus=>run(`(()=>{const approved=${kind==='approved'},q=s=>document.querySelector(s),r=e=>e?{x:e.getBoundingClientRect().x,y:e.getBoundingClientRect().y,w:e.getBoundingClientRect().width,h:e.getBoundingClientRect().height}:null,c=e=>e?getComputedStyle(e):{};const input=q(approved?'#prompt':'.cc-input');${focus?'input.focus();':'input.blur();'}return new Promise(resolve=>setTimeout(()=>{const app=q(approved?'.app':'.cc-app'),project=q(approved?'.projectbar':'.cc-projectbar'),owner=q(approved?'.outline-row .owner':'.cc-outline-row .cc-owner'),select=project.querySelector('select');resolve({width:innerWidth,documentWidth:document.documentElement.scrollWidth,appWidth:app.scrollWidth,background:c(app).backgroundColor,header:r(q(approved?'.header':'.cc-header')),project:r(project),context:r(q(approved?'.worktop':'.cc-context')),input:r(input),inputResize:c(input).resize,inputFont:c(input).fontSize,selectDisplay:select?c(select).display:null,target:q(approved?'#target':'.cc-target')?.textContent,ownerAlign:c(owner).textAlign,ownerLeftBorder:c(owner).borderLeftWidth,projectButtons:[...project.querySelectorAll('button')].filter(b=>b.getClientRects().length).map(b=>({text:b.textContent.trim(),label:b.getAttribute('aria-label'),rect:r(b)})),nav:[...document.querySelectorAll(approved?'#primary-nav button':'[data-cc-nav]')].map(b=>({text:b.textContent.trim(),font:c(b).fontSize,rect:r(b)}))});},70));})()`);
  const row={kind,width,height,light,blur:await measure(false),focus:await measure(true)};results.cases.push(row);
  if([1440,1200,390].includes(width)){await measure(false);const png=await cdp('Page.captureScreenshot',{format:'png'});fs.writeFileSync(path.join(destination,`${kind}-${width}-${light?'light':'dark'}-main.png`),Buffer.from(png.data,'base64'));}
 }
 fs.writeFileSync(path.join(destination,'measurements.json'),JSON.stringify(results,null,2)+'\n');
 console.log(JSON.stringify({cases:results.cases.length,output:destination,source}));
}finally{ws?.close();chrome.kill('SIGTERM');await new Promise(r=>chrome.exitCode!==null?r():chrome.once('exit',r));fs.rmSync(profile,{recursive:true,force:true});}
