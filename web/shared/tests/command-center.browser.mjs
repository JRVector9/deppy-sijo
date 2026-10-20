// Existing Node + installed Chrome only; no npm/browser dependency installation.
import fs from 'node:fs';
import http from 'node:http';
import os from 'node:os';
import path from 'node:path';
import {spawn} from 'node:child_process';
import {fileURLToPath} from 'node:url';

const root=path.resolve(path.dirname(fileURLToPath(import.meta.url)),'../../..');
const chrome=[process.env.CHROME_PATH,process.env.CHROME_BIN,'/Applications/Google Chrome.app/Contents/MacOS/Google Chrome','/usr/bin/google-chrome','/usr/bin/chromium'].find(p=>p&&fs.existsSync(p));
if(!chrome)throw Error('Installed Chrome required');
const fixture=String.raw`
import {createCommandCenterView} from '/command-center.js';
const check=(value,message)=>{if(!value)throw Error(message);checks++;};let checks=0;
const tick=()=>new Promise(resolve=>requestAnimationFrame(()=>requestAnimationFrame(resolve)));
const cap=(enabled=true,reason='')=>({enabled,reason});
const target=(workspace,session,task=null)=>({workspace_id:workspace,session_id:session,task_id:task});
const makeSnapshot=()=>{
 const a=target('project-a','opaque-main-a'),t=target('project-a','opaque-task-a','task-a');
 const project={id:'project-a',name:'실제 프로젝트 A',directory:'/fixture/project-a',goal:'검증용 목표',main_summary:'실제 세션 상태',pending_goal_id:'goal-a',plan_confirmation:cap(),manual_plan:cap(),planner_prompt:'호스트가 제공한 계획 요청',main_target:a,main_provider:'Codex',main_model:'configured',main_effort:'xhigh',main_input:cap(),sessions:[{target:a,title:'메인 세션 A',provider:'Codex',device:'이 Mac',state:'running',input:cap(),terminal_session_id:'11111111-1111-4111-8111-111111111111'}],tasks:[{target:t,title:'검증 작업 A',role:'구현',scope:'src',acceptance_criteria:'실제 테스트 근거',provider:'Codex',device:'이 Mac',directory:'/fixture/worktrees/a',state:'waiting_approval',activity:'승인 대기',observed_at:'14:20',result:null,result_revision:null,approval:{id:'approval-a',command:'cargo test',directory:'/fixture/worktrees/a',scope:'로컬 테스트',action:cap()},input:cap(),terminal:cap(),git:cap(false,'아직 연결되지 않았습니다'),confirm_result:cap(false,'검증 결과가 필요합니다'),dispatch:cap(),terminal_session_id:'22222222-2222-4222-8222-222222222222'}],history:[{id:'event-a',target:t,time:'14:20',text:'작업 승인 요청',detail:'실제 fixture event'}],config:{name:'실제 프로젝트 A',directory:'/fixture/project-a',main_session_id:'opaque-main-a',provider:'Codex',model:'configured',effort:'xhigh',worker_providers:['Codex'],parallel_limit:2,automation:'confirm',use_worktrees:true,revision:'1'},save_settings:cap(),available_providers:['Codex'],available_models:['configured'],available_efforts:['xhigh'],available_automation:['confirm']};
 project.config.allowed_scope_roots=['src'];project.tasks[0].eligible_sessions=[{...project.sessions[0],title:'명시적으로 고를 작업 세션',target:target('project-a','opaque-eligible-worker')}];
 const b=structuredClone(project);b.id='project-b';b.name='실제 프로젝트 B';b.main_target=target('project-b','opaque-main-b');b.sessions=[];b.tasks=[];b.history=[];b.config={...b.config,name:b.name,main_session_id:null};
 return {projects:[project,b],selected_workspace_id:'project-a',devices:[],dark_mode:true,font_scale:1,labels:{},add_project:cap(false),manage_devices:cap(false),status:'관측한 상태',receipts:[]};
};
const sent=[],watched=[],visible=[];const mount=document.getElementById('host');
const view=createCommandCenterView({host:mount,send:msg=>{sent.push(msg);return true;},watchSession:id=>watched.push(id),onVisibility:value=>visible.push(value)});
const receive=snapshot=>view.accept({type:'work_snapshot',snapshot});
const input=()=>view.element.querySelector('[data-cc-composer]');
const write=text=>{input().value=text;input().dispatchEvent(new Event('input',{bubbles:true}));};
const click=selector=>{const button=view.element.querySelector(selector);check(!!button,'missing '+selector);button.click();};
const message=async()=>{try{
 let snapshot=makeSnapshot();receive(snapshot);await tick();
 check(view.element.textContent.includes('실제 프로젝트 A'),'real snapshot project must render');
 check(view.element.querySelectorAll('[data-cc-nav]').length===5,'approved five navigation entries');
 write('한글 목표');click('[data-cc-send]');
 check(sent.length===1,'one submit');check(sent[0].intent.intent.text==='한글 목표','NFC at submission');
 check(sent[0].request===0x40000000,'reserved request namespace');
 check(sent[0].intent.intent.target.session_id==='opaque-main-a','exact opaque main target');
 check(input().value==='한글 목표','queueing must retain draft');
 view.accept({type:'cloud_control_result',request:sent[0].request,ok:true,status:'queued'});
 check(input().value.length>0,'queued transport result is not runtime ACK');
 write('편집한 초안');snapshot.receipts=[{operation_id:sent[0].intent.operation_id,status:'acknowledged',detail:'접수 확인'}];receive(snapshot);
 check(input().value==='편집한 초안','ACK must preserve newer text');
 click('[data-cc-send]');const second=sent.at(-1);
 snapshot.receipts=[{operation_id:'not-matching',status:'acknowledged',detail:''}];receive(snapshot);check(input().value==='편집한 초안','foreign ACK ignored');
 snapshot.receipts=[{operation_id:second.intent.operation_id,status:'acknowledged',detail:'접수 확인'}];receive(snapshot);check(input().value==='','matching ACK clears unchanged draft');
 write('재연결 중 보존');click('[data-cc-send]');const before=sent.length;view.resetConnection();receive({...snapshot,receipts:[]});click('[data-cc-send]');
 check(sent.length===before,'reconnect never auto resends uncertain submission');check(input().value==='재연결 중 보존','uncertain draft retained');
 click('[data-cc-project="project-b"]');write('B 초안');click('[data-cc-project="project-a"]');check(input().value==='재연결 중 보존','project draft isolation');
 click('[data-cc-project="project-b"]');check(input().value==='B 초안','second project own draft');
 click('[data-cc-project="project-a"]');
 const background=sent.length;view.accept({type:'cloud_control_result',request:42,status:'rejected'});click('[data-cc-send]');check(sent.length===background,'unrelated transport rejection cannot release operation fence');
 view.accept({type:'cloud_control_result',request:sent.at(-1).request,status:'rejected'});check(input().value==='재연결 중 보존','matching transport rejection keeps draft');
 const countBeforeIme=sent.length;write('조합');input().dispatchEvent(new CompositionEvent('compositionstart',{bubbles:true}));input().dispatchEvent(new KeyboardEvent('keydown',{key:'Enter',isComposing:true,bubbles:true,cancelable:true}));check(sent.length===countBeforeIme,'composition Enter cannot submit');input().dispatchEvent(new CompositionEvent('compositionend',{bubbles:true}));input().dispatchEvent(new KeyboardEvent('keydown',{key:'Enter',bubbles:true,cancelable:true}));check(sent.length===countBeforeIme,'same-event composition commit Enter cannot submit');await new Promise(resolve=>setTimeout(resolve,60));input().dispatchEvent(new KeyboardEvent('keydown',{key:'Enter',bubbles:true,cancelable:true}));check(sent.length===countBeforeIme+1,'normal Enter after composition submits');
 snapshot.receipts=[{operation_id:sent.at(-1).intent.operation_id,status:'rejected',detail:'거절'}];receive(snapshot);check(input().value==='조합','host rejection keeps draft');
 write('가'.repeat(5462));click('[data-cc-send]');check(sent.length===countBeforeIme+1,'UTF-8 16KiB input limit');check(input().value.length===5462,'oversized send retains full draft');write('a'.repeat(16384));click('[data-cc-send]');check(sent.length===countBeforeIme+2,'exact 16KiB accepted');
 snapshot.receipts=[{operation_id:sent.at(-1).intent.operation_id,status:'acknowledged',detail:''}];receive(snapshot);
 write('돌아온 초안');click('[data-cc-send]');const cycle=sent.at(-1);write('');write('돌아온 초안');snapshot.receipts=[{operation_id:cycle.intent.operation_id,status:'acknowledged',detail:''}];receive(snapshot);check(input().value==='돌아온 초안','edit away and back still counts as newer draft');
 const dangerous=structuredClone(snapshot);dangerous.projects[0].goal='<img src=x onerror=alert(1)>';receive(dangerous);check(!view.element.querySelector('.cc-content img'),'snapshot text is never HTML');receive(snapshot);
 for(const [width,height]of [[390,844],[760,900],[1280,900]]){
   mount.style.width=width+'px';mount.style.height=height+'px';await tick();
   for(const light of [false,true]){
     view.element.querySelector('.cc-app').classList.toggle('cc-light',light);await tick();
     check(view.element.scrollWidth<=width+1,'no page horizontal overflow '+width+' light='+light);
     check(view.element.querySelector('.cc-app').scrollWidth<=width+1,'no app horizontal overflow '+width);
   }
 }
 mount.style.width='390px';await tick();click('[data-cc-open-left]');check(view.element.querySelector('.cc-app').classList.contains('cc-left-open'),'project drawer opens');click('[data-cc-open-right]');check(!view.element.querySelector('.cc-app').classList.contains('cc-left-open'),'two drawers never open together');
 view.element.dispatchEvent(new KeyboardEvent('keydown',{key:'Escape',bubbles:true}));
 click('[data-cc-task="task-a"]');write('작업 전용');click('[data-cc-send]');check(sent.at(-1).intent.intent.target.task_id==='task-a','task-specific input target');
 const worker=view.element.querySelector('[data-cc-worker]');check(worker.value==='','dispatch never defaults to first session');worker.value='opaque-eligible-worker';worker.dispatchEvent(new Event('change',{bubbles:true}));[...view.element.querySelectorAll('.cc-actions button')].find(button=>button.textContent==='작업 실행').click();check(sent.at(-1).intent.intent.type==='dispatch_task','explicit worker dispatch');check(sent.at(-1).intent.intent.target.session_id==='opaque-eligible-worker'&&sent.at(-1).intent.intent.target.task_id==='task-a','dispatch preserves task and exact selected worker route');
 click('[data-cc-terminal="task-a"]');check(watched.at(-1)==='22222222-2222-4222-8222-222222222222','actual terminal watch UUID');
 click('[data-cc-open]');
 click('[data-cc-nav="settings"]');const roots=view.element.querySelector('[data-cc-focus="setting-allowed_scope_roots"]');roots.value='src\ntests';roots.dispatchEvent(new Event('input'));roots.focus();receive(snapshot);check(document.activeElement?.dataset.ccFocus==='setting-allowed_scope_roots'&&document.activeElement.value==='src\ntests','settings draft and focus survive snapshot');const mainBinding=view.element.querySelector('[data-cc-focus="setting-main_session_id"]');mainBinding.value='';mainBinding.dispatchEvent(new Event('change'));view.element.querySelector('.cc-form .cc-primary').click();check(sent.at(-1).intent.intent.type==='save_project'&&sent.at(-1).intent.intent.config.main_session_id===null,'settings preserves explicit unbound main');check(sent.at(-1).intent.intent.config.allowed_scope_roots.join(',')==='src,tests'&&sent.at(-1).intent.intent.config.revision==='1','scope allowlist and captured revision sent');
 click('[data-cc-nav="main"]');view.element.querySelector('.cc-manual-plan').open=true;const planArea=view.element.querySelector('.cc-plan-input');planArea.value='{}';planArea.dispatchEvent(new Event('input'));view.element.querySelector('.cc-manual-plan>button').click();const installed=sent.at(-1);check(installed.intent.intent.type==='install_plan_json'&&installed.intent.intent.workspace_id==='project-a'&&installed.intent.intent.json==='{}','manual JSON uses host validated intent');check(view.element.querySelector('.cc-plan-input').value==='{}','manual JSON retained until ACK');snapshot.receipts=[{operation_id:installed.intent.operation_id,status:'acknowledged',detail:''}];receive(snapshot);check(view.element.querySelector('.cc-plan-input').value==='','manual JSON matching ACK clears unchanged editor');
 click('[data-cc-nav="devices"]');const deviceSnapshot=structuredClone(snapshot);deviceSnapshot.devices=[{id:'viewer',name:'연결된 휴대폰',description:'열람 기기',capabilities:'열람 · 입력',connection:'연결됨',connected:true,can_execute:false}];receive(deviceSnapshot);check(view.element.querySelector('.cc-content').textContent.includes('열람 · 입력'),'native device capabilities String rendered');
 const resultSnapshot=makeSnapshot();resultSnapshot.projects[0].tasks[0].attempt_id='attempt-current';resultSnapshot.projects[0].tasks[0].result_report=cap();resultSnapshot.projects[0].tasks[0].verify_result=cap();resultSnapshot.projects[0].tasks[0].result_revision='2';resultSnapshot.projects[0].tasks[0].result='Actual result';resultSnapshot.font_scale=1.25;receive(resultSnapshot);
 click('[data-cc-task="task-a"]');[...view.element.querySelectorAll('.cc-tabs button')].find(button=>button.textContent==='결과').click();
 let reportArea=view.element.querySelector('[data-cc-result="report"]');reportArea.value='수동 결과';reportArea.dispatchEvent(new Event('input'));[...view.element.querySelectorAll('button')].find(button=>button.textContent==='결과 등록').click();let reportRequest=sent.at(-1);check(reportRequest.intent.intent.type==='report_result'&&reportRequest.intent.intent.attempt_id==='attempt-current','result form captures exact attempt');
 reportArea=view.element.querySelector('[data-cc-result="report"]');reportArea.value='등록 대기 중 편집';reportArea.dispatchEvent(new Event('input'));resultSnapshot.receipts=[{operation_id:reportRequest.intent.operation_id,status:'acknowledged',detail:''}];receive(resultSnapshot);check(view.element.querySelector('[data-cc-result="report"]').value==='등록 대기 중 편집','report ACK preserves newer editor');
 const verifyArea=view.element.querySelector('[data-cc-result="verify"]');verifyArea.value='직접 테스트 수행 결과';verifyArea.dispatchEvent(new Event('input'));[...view.element.querySelectorAll('button')].find(button=>button.textContent==='검증 통과 기록').click();check(sent.at(-1).intent.intent.type==='verify_result'&&sent.at(-1).intent.intent.revision==='2'&&sent.at(-1).intent.intent.passed===true,'manual verification captures current result revision');
 check(getComputedStyle(view.element.querySelector('.cc-work h1')).fontSize==='23.75px','browser font scale follows published setting');
 const readonly=makeSnapshot();for(const p of readonly.projects){p.main_input=cap(false,'읽기 전용');p.save_settings=cap(false,'읽기 전용');p.plan_confirmation=cap(false);p.manual_plan=cap(false);for(const t of p.tasks){t.input=cap(false);t.dispatch=cap(false);t.confirm_result=cap(false);t.approval=null;}}receive(readonly);
 click('[data-cc-nav="main"]');check(input().disabled,'read-only input disabled');check(!view.element.querySelector('[data-cc-approval]'),'no removed read-only approval');
 view.accept({type:'work_snapshot',snapshot:null});check(view.element.hidden,'null projection hides private content');view.resetConnection();view.destroy();view.destroy();check(!mount.children.length,'destroy removes DOM');
 const viewerOnly=createCommandCenterView({host:mount,readOnly:true,send:()=>{throw Error('readonly cannot send');}});viewerOnly.accept({type:'work_snapshot',snapshot:makeSnapshot()});check(viewerOnly.element.querySelector('[data-cc-composer]').disabled,'paired viewer ignores offered input capability');viewerOnly.element.querySelector('[data-cc-task="task-a"]').click();check(!viewerOnly.element.querySelector('[data-cc-approval]'),'paired viewer ignores offered approval details');viewerOnly.destroy();
 // A separate view proves bounded pending admission without inheriting the prior test operations.
 const bounded=createCommandCenterView({host:mount,send:msg=>{sent.push(msg);return true;}});const many=makeSnapshot();many.projects[0].tasks=Array.from({length:17},(_,i)=>({...many.projects[0].tasks[0],target:target('project-a','session-'+i,'task-'+i)}));bounded.accept({type:'work_snapshot',snapshot:many});const start=sent.length;
 for(let i=0;i<17;i++){bounded.element.querySelector('[data-cc-task="task-'+i+'"]').click();const field=bounded.element.querySelector('[data-cc-composer]');field.value='bounded';field.dispatchEvent(new Event('input'));bounded.element.querySelector('[data-cc-send]').click();}check(sent.length===start+16,'pending operation map is bounded at16');bounded.destroy();
 return {ok:true,checks,widths:[390,760,1280],themes:['dark','light']};
}catch(error){return {ok:false,checks,error:error.stack};}};
if(new URLSearchParams(location.search).has('visual')){receive(makeSnapshot());globalThis.ccFixture={view,receive,makeSnapshot};document.getElementById('host').style.cssText='width:100vw;height:100dvh';document.getElementById('report').hidden=true;}else{const result=await message();document.body.dataset.status=result.ok?'ok':'error';document.getElementById('report').textContent=JSON.stringify(result);fetch('/report',{method:'POST',body:JSON.stringify(result)});}
`;
let resolveReport;
const report=new Promise(resolve=>resolveReport=resolve);
const server=http.createServer(async(req,res)=>{
 if(req.url==='/reference'){res.writeHead(200,{'Content-Type':'text/html;charset=utf-8'});res.end(fs.readFileSync(path.join(root,'docs/mockups/multi-agent-command-center-v2-2026-10-20.html')));return;}
 if(req.method==='POST'&&req.url==='/report'){const chunks=[];for await(const chunk of req)chunks.push(chunk);resolveReport(JSON.parse(Buffer.concat(chunks)));res.writeHead(204);res.end();return;}
 if(req.url==='/command-center.js'||req.url==='/command-center.css'){res.writeHead(200,{'Content-Type':req.url.endsWith('.js')?'text/javascript':'text/css'});res.end(fs.readFileSync(path.join(root,'web/shared',req.url.slice(1))));return;}
 if(req.url==='/fixture.js'){res.writeHead(200,{'Content-Type':'text/javascript'});res.end(fixture);return;}
 res.writeHead(200,{'Content-Type':'text/html;charset=utf-8','Content-Security-Policy':"default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; connect-src 'self'"});res.end('<!doctype html><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><link rel="stylesheet" href="/command-center.css"><style>html,body{margin:0;background:#090c10}#host{width:390px;height:844px}#report{white-space:pre-wrap}</style><div id="host"></div><pre id="report"></pre><script type="module" src="/fixture.js"></script>');
});
await new Promise(resolve=>server.listen(0,'127.0.0.1',resolve));
if(process.argv.includes('--serve')){console.log('Command Center QA: http://127.0.0.1:'+server.address().port+'/?visual=1');console.log('Approved reference: http://127.0.0.1:'+server.address().port+'/reference');await new Promise(resolve=>{process.once('SIGTERM',resolve);process.once('SIGINT',resolve);});await new Promise(resolve=>server.close(resolve));process.exit();}
const profile=fs.mkdtempSync(path.join(os.tmpdir(),'deppy-command-center-browser-'));
const child=spawn(chrome,['--headless=new','--disable-gpu','--disable-background-networking','--no-first-run','--no-default-browser-check','--window-size=1400,1000','--user-data-dir='+profile,'http://127.0.0.1:'+server.address().port],{stdio:['ignore','ignore','pipe']});
let stderr='';child.stderr.on('data',chunk=>stderr+=chunk);
const completed=new Promise(resolve=>{child.once('error',error=>resolve({error}));child.once('exit',(code,signal)=>resolve({code,signal}));});let timer;
try{const result=await Promise.race([report,new Promise((_,reject)=>timer=setTimeout(()=>reject(Error('Chrome timeout: '+stderr)),25000)),completed.then(({error,code,signal})=>{throw error||Error('Chrome ended '+(signal||code));})]);console.log(JSON.stringify(result,null,2));if(!result.ok)process.exitCode=1;}
finally{clearTimeout(timer);child.kill('SIGTERM');await completed;await new Promise(resolve=>server.close(resolve));fs.rmSync(profile,{recursive:true,force:true});}
