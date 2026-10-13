(() => {
  localStorage.setItem('deppy.webToken', 'cloud-mobile-isolated');
  globalThis.cloudSent = [];
  globalThis.cloudErrors = [];
  window.addEventListener('error', event => cloudErrors.push(event.message));
  globalThis.cloudSnapshot = {
    captured_at: 100, truncated: true,
    connections: [{ id: 'actor-a', name: '클라우드 연구자', state: 'active', auth_kind: 'oauth', input_scope: 'read_only' }],
    approvals: [{ id: 'approval-a', connection_name: '새 연구자', scope: 'read_only', created_at: 90 }],
    runs: [
      { id: 'run-a', connection_id: 'actor-a', connection_name: '클라우드 연구자', workspace_id: 'ws-original', session_id: 'session-original', generation: 'generation-original', title: '<img src=x onerror="window.cloudInjected=1"> 긴 한글 작업 제목'.repeat(3), state: 'unknown', stale: true, local_status: 'waiting', message: '보고 연결이 끊김', created_at: 50, updated_at: 70, artifacts: [{ label: '안전 문서', uri: 'https://example.com/proof' }, { label: '차단 링크', uri: 'javascript:alert(1)' }] },
      { id: 'run-b', connection_id: 'actor-b', connection_name: '리뷰 에이전트', workspace_id: 'ws-other', session_id: 'session-other', generation: 'generation-other', title: '완료 리뷰', state: 'completed', result: '테스트 확인 완료', local_status: 'running', created_at: 40, updated_at: 60 },
    ],
    answers: [{ id: 'answer-a', connection_id: 'actor-a', connection_name: '클라우드 연구자', run_id: 'run-a', workspace_id: 'ws-original', session_id: 'session-original', generation: 'generation-original', title: '저장 답변', body: '저장된 결과 <script>window.cloudInjected=1</script>', created_at: 80 }],
  };
  class FakeSocket {
    static OPEN = 1;
    readyState = 0; listeners = new Map();
    constructor() { globalThis.cloudSocket = this; setTimeout(() => { this.readyState = 1; this.emit('open'); }, 5); }
    addEventListener(type, fn) { this.listeners.set(type, [...(this.listeners.get(type) || []), fn]); }
    emit(type, value = {}) { for (const fn of this.listeners.get(type) || []) fn(value); }
    message(frame) { this.emit('message', { data: JSON.stringify(frame) }); }
    snapshot(can_control = true, can_approve = true) { this.message({ type: 'cloud_snapshot', snapshot: cloudSnapshot, can_control, can_approve }); }
    send(raw) {
      const frame = JSON.parse(raw); cloudSent.push(frame);
      if (frame.type === 'auth') setTimeout(() => {
        this.message({ type: 'welcome', v: frame.v });
        this.message({ type: 'dashboard', workspaces: [{ id: 'ws-original', name: '원래 프로젝트', state: 'active', sessions: [{ id: 'session-original', title: '원래 세션', status: 'waiting' }] }] });
        this.snapshot();
      }, 5);
      if (frame.type === 'cloud_control' && !globalThis.cloudHoldReceipts) setTimeout(() => this.message({ type: 'cloud_control_result', request: frame.request, status: 'queued' }), 5);
    }
    close() { this.readyState = 3; this.emit('close'); }
  }
  globalThis.WebSocket = FakeSocket;
})();

(async () => {
  const sleep = ms => new Promise(resolve => setTimeout(resolve, ms));
  const waitFor = async (predicate, label) => { for (let n = 0; n < 100; n++) { if (predicate()) return; await sleep(20); } throw Error('timeout: ' + label); };
  let checks = 0;
  const check = (value, label) => { if (!value) throw Error(label); checks++; };
  const latest = () => cloudSent.filter(frame => frame.type === 'cloud_control').at(-1);
  try {
    await waitFor(() => document.querySelector('#sessions [data-session-id]'), 'authenticated dashboard');
    check(!!document.querySelector('#cloud-results'), 'cloud results view exists after operator snapshot');
    document.getElementById('cloud-results-toggle').click();
    const panel = document.getElementById('cloud-results');
    cloudSnapshot.runs[0].agent_line = 'Codex · gpt-6.1-sol · xhigh';
    cloudSnapshot.runs[0].local_status_observed_at = 95; cloudSocket.snapshot();
    check(panel.querySelector('[data-cloud-run="run-a"] .cloud-agent')?.textContent.includes('gpt-6.1-sol'), 'provider/model identity has its own field');
    check(panel.querySelector('[data-cloud-run="run-a"] .cloud-local-observed')?.textContent.includes('관측 시각'), 'local execution includes its observation time');
    for (const [state, label] of [['running','실행 중'],['waiting','입력 대기'],['needs_approval','승인 필요'],['error','오류'],['done','완료 관측'],['exited','종료됨'],['shell','셸'],['idle','유휴']]) {
      cloudSnapshot.runs[0].local_status = state; cloudSocket.snapshot();
      const observed = panel.querySelector('[data-cloud-run="run-a"] .cloud-local').textContent;
      check(observed.includes(label) && !observed.includes('gpt-6.1-sol'), 'local runtime label stays separate from model identity: ' + state);
    }
    cloudSnapshot.runs[0].local_status = 'waiting'; cloudSnapshot.runs[0].local_status_observed_at = null; cloudSocket.snapshot();
    check(panel.querySelector('[data-cloud-run="run-a"] .cloud-local-observed').textContent.includes('관측 시각 정보 없음'), 'missing timestamp never implies a fresh local observation');
    const originalRun = panel.querySelector('[data-cloud-run="run-a"]');
    const focusedControl = originalRun.querySelector('[data-cloud-action="take_control"]');
    focusedControl.focus();
    const selectedBody = originalRun.querySelector('.cloud-body').firstChild;
    const selection = window.getSelection();
    selection.setBaseAndExtent(selectedBody, 0, selectedBody, selectedBody.length);
    const selectedText = selection.toString();
    check(document.activeElement === focusedControl && selectedText.length > 0, 'real focus and result selection established');
    cloudSnapshot.captured_at++; cloudSocket.snapshot();
    check(panel.querySelector('[data-cloud-run="run-a"]') === originalRun && document.activeElement === focusedControl && selection.toString() === selectedText, 'timestamp-only snapshot preserves keyed card, focus, and selected text');
    cloudSnapshot.runs[0].progress = 25; cloudSnapshot.runs[0].local_status = 'running'; cloudSocket.snapshot();
    check(originalRun.isConnected && document.activeElement === focusedControl && selection.toString() === selectedText && originalRun.querySelector('.cloud-reported').textContent.includes('25%'), 'actual status update preserves unrelated focused and selected nodes');
    cloudSnapshot.runs[0].message += ' · 결과 확인'; cloudSocket.snapshot();
    check(originalRun.querySelector('.cloud-body').firstChild === selectedBody && selection.anchorOffset === 0 && selection.focusOffset === selectedText.length && selection.toString() === selectedText, 'selected body update preserves text node and selection offsets');
    cloudSnapshot.runs.unshift({ ...cloudSnapshot.runs[1], id:'run-inserted', title:'새 작업' }); cloudSocket.snapshot();
    check(document.activeElement === focusedControl && selection.toString() === selectedText && originalRun.isConnected, 'inserting another keyed row preserves focus and selection');
    cloudSnapshot.runs.shift(); cloudSocket.snapshot();
    cloudSocket.snapshot(false, false);
    const deniedCount = cloudSent.length; focusedControl.click();
    check(focusedControl.disabled && cloudSent.length === deniedCount && originalRun.isConnected, 'retained button receives revoked authority before interaction');
    cloudSocket.snapshot(true, true);
    cloudSnapshot.runs[0].generation = 'generation-refreshed'; cloudSocket.snapshot();
    focusedControl.click();
    check(latest().intent.generation === 'generation-refreshed', 'retained control uses latest generation intent');
    await sleep(25);
    cloudSnapshot.runs[0].generation = 'generation-original'; cloudSnapshot.runs[0].local_status = 'waiting'; cloudSocket.snapshot();
    cloudSnapshot.runs[0].artifacts.push({ label:'중복 문서', uri:'https://example.com/proof' });
    for (let update = 0; update < 3; update++) cloudSocket.snapshot();
    check(originalRun.querySelectorAll('a').length === 2, 'repeated artifact keys cannot accumulate stale links across snapshots');
    cloudSnapshot.runs[0].artifacts.pop(); cloudSocket.snapshot();
    check(!panel.hidden, 'results view opens');
    check(getComputedStyle(document.getElementById('workspace-search').closest('label')).display === 'none', 'workspace search is hidden while cloud view opens');
    document.documentElement.style.width = '390px'; await sleep(30);
    check(panel.scrollWidth <= panel.clientWidth + 1, '390px results have no horizontal overflow');
    check(!panel.querySelector('script,img') && !window.cloudInjected, 'untrusted titles and bodies render as text');
    check(panel.textContent.includes('클라우드 보고') && panel.textContent.includes('로컬 터미널'), 'cloud and local states are independent labels');
    check(panel.textContent.includes('보고가 오래됨') && panel.textContent.includes('상태 미확인'), 'stale unknown remains explicit');
    check(panel.textContent.includes('최근 항목'), 'bounded history is disclosed');
    cloudSnapshot.connections[0].input_scope = 'read+input'; cloudSocket.snapshot();
    check(panel.querySelector('.cloud-connection').textContent.includes('입력 허용'), 'native input-scope value has an explicit input label');
    cloudSnapshot.connections[0].state = 'waiting'; cloudSocket.snapshot();
    check(panel.querySelector('.cloud-connection').textContent.includes('인증 대기'), 'listener-only connection is never labeled authenticated');
    cloudSnapshot.connections[0].state = 'active'; cloudSocket.snapshot();
    check(panel.querySelectorAll('a').length === 1 && panel.querySelector('a').rel.includes('noopener'), 'only safe artifact URI is clickable');
    const search = panel.querySelector('input[type=search]'); search.value = '완료 리뷰'; search.dispatchEvent(new Event('input'));
    check(panel.querySelectorAll('[data-cloud-run]').length === 1 && panel.textContent.includes('완료 리뷰'), 'recent work title search');
    search.value = ''; search.dispatchEvent(new Event('input'));
    const filter = panel.querySelector('select'); filter.value = 'active'; filter.dispatchEvent(new Event('change'));
    check(panel.querySelectorAll('[data-cloud-run]').length === 1, 'active filter retains uncertain work and excludes terminal report');
    filter.value = 'all'; filter.dispatchEvent(new Event('change'));
    panel.querySelector('[data-cloud-run="run-a"] [data-cloud-action="cancel_run"]').click();
    check(latest().intent.connection_id === 'actor-a' && latest().intent.run_id === 'run-a', 'cancel carries original actor and run');
    await sleep(25);
    check(panel.querySelector('[data-cloud-run="run-a"]').textContent.includes('상태 미확인'), 'queue admission cannot mark cloud run cancelled');
    cloudSnapshot.runs[0].cancel_requested_at = 101; cloudSocket.snapshot();
    check(panel.querySelector('[data-cloud-run="run-a"]').textContent.includes('취소 요청됨'), 'authoritative cancel request shown');
    cloudSnapshot.runs[0].cancel_acknowledged_at = 102; cloudSocket.snapshot();
    check(panel.querySelector('[data-cloud-run="run-a"]').textContent.includes('취소 확인됨'), 'same-actor cancellation acknowledgment shown');
    panel.querySelector('[data-cloud-run="run-a"] [data-cloud-action="take_control"]').click();
    check(latest().intent.session_id === 'session-original' && latest().intent.generation === 'generation-original', 'takeover retains exact generation');
    panel.querySelector('[data-cloud-action="revoke_connection"]').click();
    check(latest().intent.connection_id === 'actor-a', 'revoke targets exact connection');
    panel.querySelector('[data-cloud-action="approve_connection"]').click();
    check(latest().intent.approval_id === 'approval-a' && latest().intent.allow === true, 'approval carries explicit allow');
    cloudSocket.snapshot(false, false);
    check(!panel.textContent.includes('새 연구자'), 'ungranted connection approvals are omitted from read-only view');
    check(Array.from(panel.querySelectorAll('[data-cloud-action]')).every(button => button.disabled), 'read-only controls disabled including navigation that switches Mac');
    const count = cloudSent.length; panel.querySelector('[data-cloud-action="revoke_connection"]').click();
    check(cloudSent.length === count, 'disabled control creates no effect');
    cloudSocket.snapshot(true, true);
    panel.querySelector('[data-cloud-run="run-a"] [data-cloud-action="navigate"]').click();
    check(latest().intent.workspace_id === 'ws-original' && latest().intent.session_id === 'session-original', 'navigation carries exact original target');
    document.getElementById('viewer-back').click(); await sleep(30);
    const dashboard = (originalState = 'warm', originalWorkspace = 'ws-original') => cloudSocket.message({ type:'dashboard', workspaces:[
      { id:originalWorkspace, name:'원래 프로젝트', state:originalState, sessions:[{ id:originalState === 'active' ? 'session-original' : null, selection_id:'session-original', title:'원래 세션', status:'waiting' }] },
      { id:'ws-other', name:'다른 프로젝트', state:'active', sessions:[{ id:'session-other', title:'다른 세션', status:'running' }] },
    ] });
    const selectOther = () => document.querySelector('#sessions [data-session-id="session-other"]').click();
    const lastWatch = () => cloudSent.filter(frame => frame.type === 'watch').at(-1)?.session;
    dashboard();
    panel.querySelector('[data-cloud-run="run-a"] [data-cloud-action="navigate"]').click(); await sleep(25);
    selectOther(); dashboard('active'); await sleep(25);
    check(lastWatch() === 'session-other', 'explicit selection supersedes queued original-session navigation');
    document.getElementById('viewer-back').click(); await sleep(30); dashboard();
    globalThis.cloudHoldReceipts = true;
    panel.querySelector('[data-cloud-run="run-a"] [data-cloud-action="navigate"]').click(); const obsoleteRequest = latest().request;
    selectOther(); cloudSocket.message({ type:'cloud_control_result', request:obsoleteRequest, status:'queued' }); dashboard('active'); await sleep(25);
    check(lastWatch() === 'session-other', 'late navigation receipt cannot supersede a newer selection');
    document.getElementById('viewer-back').click(); await sleep(30); dashboard(); globalThis.cloudHoldReceipts = false;
    panel.querySelector('[data-cloud-run="run-a"] [data-cloud-action="navigate"]').click(); await sleep(25);
    const watchCount = cloudSent.filter(frame => frame.type === 'watch').length;
    dashboard('active', 'wrong-workspace'); await sleep(25);
    check(cloudSent.filter(frame => frame.type === 'watch').length === watchCount, 'pending navigation cannot borrow a matching session from another workspace');
    dashboard();
    panel.querySelector('[data-cloud-run="run-a"] [data-cloud-action="navigate"]').click(); await sleep(25);
    panel.querySelector('[data-cloud-run="run-b"] [data-cloud-action="navigate"]').click(); await sleep(25);
    dashboard('active'); await sleep(25);
    check(lastWatch() === 'session-other', 'latest cloud navigation supersedes the previous pending target');
    document.getElementById('viewer-back').click(); await sleep(30); dashboard();
    panel.querySelector('[data-cloud-run="run-a"] [data-cloud-action="navigate"]').click(); await sleep(25);
    const disconnected = cloudSocket;
    const originalWatchCount = cloudSent.filter(frame => frame.type === 'watch' && frame.session === 'session-original').length;
    disconnected.close(); await waitFor(() => cloudSocket !== disconnected && cloudSocket.readyState === 1, 'new socket'); await sleep(40);
    check(cloudSent.filter(frame => frame.type === 'watch' && frame.session === 'session-original').length === originalWatchCount, 'disconnected navigation never follows into a new socket');
    check(!cloudSent.some(frame => ['input','direct_input','key','direct_key'].includes(frame.type)), 'cloud operator controls never send terminal input');
    check(cloudErrors.length === 0, 'production shell has no browser errors: ' + cloudErrors.join('; '));
    document.body.dataset.status = 'ok'; document.body.append('CLOUD_RESULTS_MOBILE_OK:' + checks);
  } catch (error) { document.body.dataset.status = 'error'; document.body.append('CLOUD_RESULTS_MOBILE_ERROR:' + (error.stack || error)); }
})();
