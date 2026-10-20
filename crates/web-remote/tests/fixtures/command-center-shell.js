(() => {
  localStorage.setItem('deppy.webToken', 'isolated-command-center');
  const cap = {enabled: true, reason: ''};
  const route = {workspace_id: 'project-a', session_id: 'opaque-main', task_id: null};
  globalThis.ccErrors = []; globalThis.ccSent = [];
  window.addEventListener('error', event => ccErrors.push(event.message));
  globalThis.ccSnapshot = {projects: [{id: 'project-a', name: '독립 프로젝트', directory: '/fixture/한글', goal: '실제 목표', main_summary: '관측된 상태', main_target: route, main_provider: 'Codex', main_input: cap, sessions: [{target: route, title: '현재 메인', provider: 'Codex', state: 'queued', input: cap, terminal_session_id: '11111111-1111-4111-8111-111111111111'}], tasks: [], history: [], config: {}, save_settings: cap, available_providers: [], available_models: [], available_efforts: []}], selected_workspace_id: 'project-a', devices: [], dark_mode: true, labels: {}, status: '', receipts: []};
  class FakeSocket {
    static OPEN = 1; static CONNECTING = 0;
    readyState = 0; listeners = new Map();
    constructor() { globalThis.ccSocket = this; setTimeout(() => { this.readyState = 1; this.emit('open'); }, 5); }
    addEventListener(type, handler) { this.listeners.set(type, [...(this.listeners.get(type) || []), handler]); }
    emit(type, value = {}) { for (const handler of this.listeners.get(type) || []) handler(value); }
    message(frame) { this.emit('message', {data: JSON.stringify(frame)}); }
    publish(snapshot = ccSnapshot) { this.message({type: 'work_snapshot', snapshot}); }
    send(raw) {
      const frame = JSON.parse(raw); ccSent.push(frame);
      if (frame.type === 'auth') setTimeout(() => { this.message({type: 'welcome', v: frame.v}); this.message({type: 'dashboard', workspaces: [{id: 'project-a', name: '독립 프로젝트', state: 'active', sessions: [{id: '11111111-1111-4111-8111-111111111111', title: '현재 메인', agent: 'Codex', status: 'waiting', exited: false}]}]}); this.publish(); }, 5);
      if (frame.type === 'cloud_control') this.message({type: 'cloud_control_result', request: frame.request, status: 'queued'});
      if (frame.type === 'watch') setTimeout(() => this.message({type: 'viewport', session: frame.session, seq: 1, keyframe: true, cols: 80, rows: 2, offset: 0, alt: false, cursor: {row: 1, col: 0, visible: true}, lines: [{row: 0, runs: [{s: 0, t: '독립 터미널'}]}, {row: 1, runs: []}]}), 5);
    }
    close() { this.readyState = 3; this.emit('close'); }
  }
  globalThis.WebSocket = FakeSocket;
})();
(async () => {
  const sleep = ms => new Promise(resolve => setTimeout(resolve, ms));
  const wait = async (fn, label) => { for (let i = 0; i < 150; i++) { if (fn()) return; await sleep(20); } throw Error('timeout: ' + label); };
  let checks = 0;
  const check = (value, label) => { if (!value) throw Error(label); checks++; };
  const host = () => document.getElementById('command-center-host');
  const input = () => host()?.querySelector('[data-cc-composer]');
  try {
    await wait(() => input() && !input().disabled && host().querySelector('[data-cc-project]'), 'authenticated projection');
    check(document.getElementById('dashboard-shell').hidden && !host().hidden, 'new projection replaces dashboard');
    check(host().getBoundingClientRect().height === innerHeight, 'production host fills viewport height');
    const write = text => { input().value = text; input().dispatchEvent(new Event('input', {bubbles: true})); };
    write('한글 목표'); host().querySelector('[data-cc-send]').click();
    const request = ccSent.find(frame => frame.type === 'cloud_control');
    check(request.intent.intent.target.session_id === 'opaque-main' && request.intent.intent.text === '한글 목표', 'exact route and NFC cross production bundle');
    check(input().value.length > 0, 'transport queue does not clear draft');
    write('저장 중 새 초안'); ccSnapshot.receipts = [{operation_id: request.intent.operation_id, status: 'acknowledged', detail: '접수'}]; ccSocket.publish();
    check(input().value === '저장 중 새 초안', 'newer draft survives matching ACK');
    host().querySelector('.cc-session').click();
    await wait(() => !document.getElementById('viewer').hidden, 'terminal open');
    check(host().hidden, 'terminal and command center do not overlap');
    check(ccSent.find(frame => frame.type === 'watch').session === '11111111-1111-4111-8111-111111111111', 'actual UUID watch');
    document.getElementById('viewer-back').click();
    await wait(() => !host().hidden && !host().querySelector('.cc-app').hidden, 'return to command center');
    check(input().value === '저장 중 새 초안', 'terminal roundtrip preserves target draft');
    [...host().querySelectorAll('.cc-head-actions button')].find(button => button.textContent === '터미널로').click();
    check(!document.getElementById('dashboard-shell').hidden && !host().hidden, 'original shell and reopen control remain reachable');
    host().querySelector('[data-cc-open]').click();
    check(document.getElementById('dashboard-shell').hidden, 'reentry hides original shell');
    ccSocket.publish(null);
    check(host().hidden && !document.getElementById('dashboard-shell').hidden, 'rollback removes private projection');
    ccSocket.publish();
    check(!host().hidden && !host().querySelector('[data-cc-open]').hidden, 'new projection exposes reopen after rollback');
    check(ccErrors.length === 0, 'no production JavaScript error: ' + ccErrors.join(', '));
    const report = document.createElement('pre'); report.textContent = 'COMMAND_CENTER_SHELL_OK:' + checks; document.body.append(report); document.body.dataset.status = 'ok';
  } catch (error) { const report = document.createElement('pre'); report.textContent = error.stack; document.body.append(report); document.body.dataset.status = 'error'; }
})();
