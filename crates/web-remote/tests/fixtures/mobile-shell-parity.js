(() => {
  localStorage.setItem('deppy.webToken', 'test-token');
  globalThis.mobileSent = [];
  class FakeSocket {
    static OPEN = 1;
    constructor() {
      this.readyState = 0;
      this.listeners = new Map();
      setTimeout(() => {
        this.readyState = FakeSocket.OPEN;
        this.dispatch('open');
        this.dispatch('message', { data: JSON.stringify({ type: 'welcome', v: 4 }) });
        this.dispatch('message', { data: JSON.stringify({
          type: 'dashboard',
          workspaces: [{ id: 'ws-1', name: 'Design', current_directory: '/Users/jr/Design', state: 'active', sessions: [
            { id: 's-1', title: 'Claude', status: 'running', exited: false },
            { id: 's-2', title: 'Codex', status: 'idle', exited: false },
          ] }],
        }) });
        this.dispatch('message', { data: JSON.stringify({
          type: 'approvals', pending: [{ id: 'a-1', server: 'test', tool: 'read_file',
            session: 's-1', preview: '검토할 내용' }],
        }) });
      }, 20);
    }
    addEventListener(type, listener) {
      const handlers = this.listeners.get(type) || [];
      handlers.push(listener);
      this.listeners.set(type, handlers);
    }
    dispatch(type, event = {}) {
      for (const listener of this.listeners.get(type) || []) listener(event);
    }
    send(value) { mobileSent.push(JSON.parse(value)); }
    close() { this.readyState = 3; }
  }
  globalThis.WebSocket = FakeSocket;
})();

const waitFor = (condition) => new Promise((resolve, reject) => {
  const deadline = Date.now() + 3000;
  const poll = () => {
    if (condition()) return resolve();
    if (Date.now() > deadline) return reject(new Error('timed out waiting for mobile shell'));
    setTimeout(poll, 20);
  };
  poll();
});
const check = (condition, message) => { if (!condition) throw new Error(message); };

async function run() {
  await waitFor(() => document.querySelector('.ws-group'));
  check(document.getElementById('workspace-count').textContent === '1 워크스페이스', 'workspace count');
  check(document.getElementById('status-text').textContent === '연결됨', 'connection subtitle');
  check(getComputedStyle(document.querySelector('.brand')).gridTemplateColumns.split(' ').length === 2,
    'two-column mobile top bar');
  check(document.querySelector('.ws-group .ws-name').textContent === 'Design', 'workspace card');
  check(document.querySelector('.ws-group .ws-path').textContent === '/Users/jr/Design', 'workspace path');
  check(document.querySelectorAll('.ws-group .session').length === 2, 'terminal chips');
  check(!document.getElementById('approval-toggle').hidden, 'pending approval indicator');
  check(document.getElementById('approval-panel').hidden, 'approval panel initially collapsed');
  document.getElementById('approval-toggle').click();
  check(!document.getElementById('approval-panel').hidden, 'approval panel opens');
  document.querySelector('.approval-card .allow').click();
  check(mobileSent.some((frame) => frame.type === 'resolve' && frame.id === 'a-1' && frame.allowed),
    'approval action stays connected');
  document.getElementById('approval-toggle').click();
  document.querySelector('.ws-group').click();
  check(!document.getElementById('viewer').hidden, 'terminal opens from workspace card');
  check(document.getElementById('viewer-title').textContent === 'Design', 'terminal workspace title');
  check(document.getElementById('viewer-session-chip').textContent === 'Claude', 'terminal title chip');
  check(getComputedStyle(document.querySelector('.composer')).display === 'grid', 'compact four-column composer');
  const composer = document.getElementById('composer-text');
  composer.value = '모바일 입력';
  composer.dispatchEvent(new Event('input', { bubbles: true }));
  document.getElementById('composer-send').click();
  check(mobileSent.some((frame) => frame.type === 'input' && frame.session === 's-1'
    && frame.text === '모바일 입력' && frame.submit), 'composer send stays connected');
  check(document.getElementById('viewer-quick-actions').hidden, 'quick actions initially hidden');
  document.getElementById('viewer-quick-button').click();
  check(!document.getElementById('viewer-quick-actions').hidden, 'quick actions open from slash');
  document.getElementById('viewer-session-chip').click();
  check(document.getElementById('viewer-session-chip').textContent === 'Codex', 'terminal cycles');
  check(mobileSent.some((frame) => frame.type === 'watch' && frame.session === 's-2'), 'cycled watch sent');
  check(getComputedStyle(document.getElementById('viewer')).backgroundColor === 'rgb(5, 5, 5)', 'terminal shell theme');
  document.body.dataset.status = 'ok';
  document.body.append('MOBILE_SHELL_PARITY_OK');
}
run().catch((error) => {
  document.body.dataset.status = 'error';
  document.body.append('MOBILE_SHELL_PARITY_ERROR:' + error.message);
});
