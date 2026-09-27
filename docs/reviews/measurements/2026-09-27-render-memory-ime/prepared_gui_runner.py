import subprocess,time,json,pathlib,os,statistics,hashlib
ROOT=pathlib.Path('/tmp/deppy-live-measure-20260927')
BINARY=pathlib.Path('/Users/jr/Desktop/projects/deppy-sijo-cloud-agent-mcp/target/release/deppy-sijo')
SCENARIOS=[('idle',1),('idle',8),('dirty1',1),('bulk',1),('agenttui',1),('fullscreen',1),('switch',8),('createdelete',1)]

def seconds(text):
    return sum(float(v)*60**i for i,v in enumerate(reversed(text.split(':'))))

def sample(pid):
    output=subprocess.run(['ps','-p',str(pid),'-o','time=,rss='],capture_output=True,text=True)
    parts=output.stdout.split()
    if len(parts)!=2:return None
    return {'t':time.monotonic(),'cpu_s':seconds(parts[0]),'rss_kib':int(parts[1])}

def quantile(values,q):
    values=sorted(values)
    return values[min(len(values)-1,int(q*(len(values)-1)))] if values else None

manifest={'binary':str(BINARY),'sha256':hashlib.sha256(BINARY.read_bytes()).hexdigest(),'git_head':subprocess.check_output(['git','-C',str(BINARY.parents[2]),'rev-parse','HEAD'],text=True).strip(),'allocator':'normal release/mimalloc; no bench-alloc','seconds_each':18,'existing_app_pid':71086,'scenarios':SCENARIOS,'started_at':time.strftime('%Y-%m-%dT%H:%M:%S%z')}
(ROOT/'manifest.json').write_text(json.dumps(manifest,indent=2))
results=[]
for scenario,workspaces in SCENARIOS:
    name=f'{scenario}-ws{workspaces}'
    env={key:value for key,value in os.environ.items() if not key.startswith('DEPPY_')}
    env.update(DEPPY_RENDER_BENCH='1',DEPPY_BENCH_SCENARIO=scenario,DEPPY_BENCH_WORKSPACES=str(workspaces),DEPPY_BENCH_SECS='18',DEPPY_BENCH_ITERS='12',DEPPY_FRAME_STATS='1',DEPPY_RESOURCE_STATS='1',DEPPY_BENCH_OUT=str(ROOT/f'{name}.jsonl'))
    env.pop('DEPPY_ALLOC_STATS',None)
    samples=[]
    with (ROOT/f'{name}.stdout.log').open('w') as logfile:
        p=subprocess.Popen([str(BINARY)],cwd=str(BINARY.parents[2]),env=env,stdout=logfile,stderr=subprocess.STDOUT)
        started=time.monotonic()
        while p.poll() is None:
            value=sample(p.pid)
            if value:samples.append(value)
            if time.monotonic()-started>60:
                (ROOT/'hung.json').write_text(json.dumps({'pid':p.pid,'scenario':name}))
                raise RuntimeError(f'Bench {name} exceeded60s; remaining runs stopped. Existing app untouched. Own bench PID={p.pid}')
            time.sleep(.5)
    (ROOT/f'{name}.ps.json').write_text(json.dumps(samples,indent=2))
    events=[json.loads(line) for line in (ROOT/f'{name}.jsonl').read_text().splitlines() if line.strip()] if (ROOT/f'{name}.jsonl').exists() else []
    frames=[e for e in events if e.get('ev')=='frame']
    rss=[e for e in events if e.get('ev')=='rss']
    measured=[s for s in samples if s['t']-started>=5]
    cpu=(measured[-1]['cpu_s']-measured[0]['cpu_s'])/(measured[-1]['t']-measured[0]['t'])*100 if len(measured)>1 else None
    result={'name':name,'pid':p.pid,'exit_code':p.returncode,'wall_s':time.monotonic()-started,'cpu_after5_core_pct':cpu,'rss_after5_mib_max':max(s['rss_kib'] for s in measured)/1024 if measured else None,'frame_count':len(frames),'ui_ms_p50':quantile([e['ui_ms'] for e in frames],.5),'ui_ms_p95':quantile([e['ui_ms'] for e in frames],.95),'rows_rebuilt_sum':sum(e['rows_rebuilt'] for e in frames),'rows_painted_sum':sum(e['rows_painted'] for e in frames),'shapes_p50':quantile([e['shapes'] for e in frames],.5),'rss_last_event':rss[-1] if rss else None}
    results.append(result)
    (ROOT/'summary.json').write_text(json.dumps(results,indent=2))
    print(json.dumps(result),flush=True)
    if p.returncode!=0:raise RuntimeError(f'Bench failed: {name}; see stdout.log')
print('ALL_ISOLATED_BENCHES_EXITED',flush=True)
