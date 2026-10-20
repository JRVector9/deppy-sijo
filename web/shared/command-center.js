// Projection only: the host authorizes every action and supplies exact opaque routes.
const INPUT_LIMIT = 16 * 1024, PLAN_LIMIT = 128 * 1024;
const DRAFT_LIMIT = 1024 * 1024, TOTAL_DRAFT_LIMIT = 32 * 1024 * 1024;
// Shared across view lifetimes so a late transport rejection cannot match a new view.
let nextRequest = 0x40000000;
const encoder = new TextEncoder();
const bytes = value => encoder.encode(value).length;
const str = value => typeof value === 'string' ? value : '';
const list = value => Array.isArray(value) ? value : [];
const copy = value => JSON.parse(JSON.stringify(value));
const routeKey = target => target ? JSON.stringify([target.workspace_id, target.session_id ?? null, target.task_id ?? null]) : '';
const states = {
  plan_review: ['계획 검토', 'amber'], queued: ['실행 대기', 'muted'], running: ['실행 중', 'accent'],
  waiting_approval: ['승인 대기', 'amber'], waiting_input: ['입력 대기', 'amber'],
  result_submitted: ['결과 제출', 'amber'], verification_failed: ['검증 실패', 'red'],
  awaiting_user_confirmation: ['확인 대기', 'amber'], completed: ['확인 완료', 'green'],
  failed: ['실패', 'red'], cancelled: ['취소됨', 'muted'], observation_unknown: ['관측 불확실', 'amber'],
};
function node(tag, className = '', value) {
  const element = document.createElement(tag);
  element.className = className;
  if (value !== undefined) element.textContent = String(value);
  return element;
}
function button(caption, action, className = 'cc-button') {
  const control = node('button', className, caption);
  control.type = 'button'; control.addEventListener('click', action); return control;
}
function stateTag(state, translate = (_, fallback) => fallback) {
  const [caption, tone] = states[state] || ['관측 불확실', 'amber'];
  const keys = {waiting_approval: 'approval', waiting_input: 'input', result_submitted: 'result', verification_failed: 'verification', awaiting_user_confirmation: 'confirmation', observation_unknown: 'unknown'};
  return node('span', 'cc-state cc-' + tone, translate('state_' + (keys[state] || state), caption));
}
function facts(entries) {
  const result = node('dl', 'cc-facts');
  for (const [caption, value] of entries) result.append(node('dt', '', caption), node('dd', '', value || '—'));
  return result;
}
function textSelection(control) {
  if (!control || !(control.tagName === 'TEXTAREA' || control.tagName === 'INPUT' && ['text', 'search', 'url', 'tel', 'password'].includes(control.type))) return null;
  return Number.isInteger(control.selectionStart) && Number.isInteger(control.selectionEnd) ? [control.selectionStart, control.selectionEnd, control.selectionDirection] : null;
}

export function createCommandCenterView({host, send, watchSession, onVisibility, readOnly = false} = {}) {
  if (!host?.append) throw new TypeError('Command Center host element required');
  const enabled = capability => !readOnly && capability?.enabled === true;
  let snapshot = null, workspaceId = null, screen = 'main', taskId = null, detailTab = 'work';
  let connected = false, opened = false, initialized = false, destroyed = false, composing = false;
  let currentKey = '', currentTarget = null, currentCapability = null, draftTotal = 0, draftSerial = 0, compositionEndedAt = -Infinity;
  let themeOverride = null, drawer = null, drawerReturn = null, lastVisibility = null, monitorFilter = 'all', monitorCollapsed = false;
  let remoteWatchTimer = null;
  const drafts = new Map(), pending = new Map(), settings = new Map(), plans = new Map(), resultDrafts = new Map(), workerSelections = new Map(), remotePositions = new Map();
  const element = node('section', 'cc-shell'); element.hidden = true;
  const reopen = button('지휘 터미널 열기', () => setOpen(true), 'cc-button cc-reopen'); reopen.dataset.ccOpen = '';
  const app = node('section', 'cc-app'); app.setAttribute('aria-label', '에이전트 지휘 터미널');
  const header = node('header', 'cc-header'), brand = node('div', 'cc-brand');
  brand.append(node('span', 'cc-mark', 'D'), node('span', 'cc-brand-name', 'Deppy Sijo'));
  const nav = node('nav', 'cc-nav'); nav.setAttribute('aria-label', '주요 화면');
  const headActions = node('div', 'cc-head-actions');
  const theme = button('라이트', () => { themeOverride = !app.classList.contains('cc-light'); applyTheme(); }, 'cc-button cc-ghost');
  const currentTerminal = button('터미널로', openCurrentTerminal, 'cc-button cc-ghost'); currentTerminal.dataset.ccCurrentTerminal = '';
  const close = button('×', () => setOpen(false), 'cc-button cc-ghost'); close.dataset.ccClose = ''; close.setAttribute('aria-label', '지휘 화면 접기');
  headActions.append(theme, currentTerminal, close);
  header.append(brand, nav, headActions);
  const layout = node('div', 'cc-layout');
  const backdrop = button('', closeDrawers, 'cc-backdrop'); backdrop.tabIndex = -1; backdrop.setAttribute('aria-label', '패널 닫기');
  const left = node('aside', 'cc-sidebar'); left.setAttribute('aria-label', '프로젝트와 독립 세션');
  const workspace = node('div', 'cc-workspace'), projectbar = node('header', 'cc-projectbar');
  const workbody = node('div', 'cc-workbody'), main = node('main', 'cc-work');
  const context = node('div', 'cc-context'), content = node('div', 'cc-content'); content.tabIndex = -1;
  const form = node('form', 'cc-composer-wrap'), composer = node('div', 'cc-composer');
  const input = node('textarea', 'cc-input'); input.rows = 2; input.dataset.ccComposer = '';
  const composerFoot = node('div', 'cc-composer-foot'), targetCaption = node('span', 'cc-target'), inputHint = node('span', 'cc-input-hint');
  const submit = node('button', 'cc-button cc-primary', '보내기 ↑'); submit.type = 'submit'; submit.dataset.ccSend = '';
  composerFoot.append(targetCaption, inputHint, submit); composer.append(input, composerFoot); form.append(composer);
  main.append(context, content, form);
  const right = node('aside', 'cc-monitor'); right.setAttribute('aria-label', '현재 프로젝트 작업자 모니터');
  workbody.append(main, right); workspace.append(projectbar, workbody); layout.append(backdrop, left, workspace);
  const footer = node('footer', 'cc-status'), status = node('span', 'cc-status-text'); footer.append(status);
  const notice = node('div', 'cc-notice'); notice.hidden = true; notice.setAttribute('role', 'status'); notice.setAttribute('aria-live', 'polite');
  app.append(header, layout, footer, notice); element.append(reopen, app); host.append(element);
  const label = (key, fallback) => str(snapshot?.labels?.['command_center.' + key]) || fallback;
  const project = () => list(snapshot?.projects).find(p => p.id === workspaceId) || null;
  const selectedTask = () => list(project()?.tasks).find(t => t.target?.task_id === taskId) || null;
  const say = message => { notice.textContent = message; notice.hidden = !message; };
  const notifyVisibility = value => { if (value !== lastVisibility) { lastVisibility = value; onVisibility?.(value); } };
  const isPending = scope => [...pending.values()].some(item => item.scope === scope);
  function applyTheme() {
    const fontScale = Number(snapshot?.font_scale);
    app.style.setProperty('--cc-font-scale', Number.isFinite(fontScale) ? Math.max(.75, Math.min(1.5, fontScale)) : 1);
    app.classList.toggle('cc-light', themeOverride ?? snapshot?.dark_mode === false);
    theme.textContent = app.classList.contains('cc-light') ? label('dark', '다크') : label('light', '라이트');
    theme.setAttribute('aria-label', '이 브라우저에서 ' + theme.textContent + ' 테마 사용');
  }
  function setOpen(value) {
    if (!snapshot || destroyed) return;
    rememberRemotePosition();
    opened = value; app.hidden = !value; reopen.hidden = value;
    if (!value) closeDrawers(); notifyVisibility(value); restoreRemotePosition(); syncRemoteWatch();
  }
  function rememberDraft() {
    if (!currentKey) return true;
    const previous = drafts.get(currentKey), size = bytes(input.value);
    if (size > DRAFT_LIMIT || draftTotal - (previous?.size || 0) + size > TOTAL_DRAFT_LIMIT || (!previous && drafts.size >= 256 && size)) {
      say('초안 저장 한도를 넘었습니다. 내용을 복사하거나 다른 초안을 비운 뒤 이동해 주세요.'); return false;
    }
    if (previous?.text === input.value) return true;
    draftTotal += size - (previous?.size || 0);
    if (!size) drafts.delete(currentKey);
    else drafts.set(currentKey, {text: input.value, size, version: ++draftSerial});
    return true;
  }
  function emit(intent, scope, metadata = {}) {
    // Observation is a repeatable read, unlike input/launch operations. A lost read receipt
    // must not permanently fence the watch, nor retire any mutating operation's fence.
    for (const [id, item] of pending) if (item.observation && performance.now() - item.started >= 2000) pending.delete(id);
    if (!connected || destroyed || isPending(scope)) return false;
    if (pending.size >= 16) { say('접수 확인 대기 항목이 많습니다. 기존 요청을 확인한 뒤 다시 시도해 주세요.'); return false; }
    if (nextRequest > 0xffffffff) { say('요청 번호를 모두 사용했습니다. 연결을 새로 열어 주세요.'); return false; }
    if (!globalThis.crypto?.randomUUID) { say('보안 연결에서만 요청을 전송할 수 있습니다.'); return false; }
    const operation = crypto.randomUUID(), item = {scope, request: nextRequest++, status: 'pending', started: performance.now(), observation: intent.type === 'observe_remote_session', ...metadata};
    pending.set(operation, item);
    try {
      const result = send?.({type: 'cloud_control', request: item.request, intent: {action: 'command_center', operation_id: operation, intent: copy(intent)}});
      if (result === false || typeof send !== 'function') { pending.delete(operation); say('연결이 요청을 접수하지 못했습니다. 초안은 보존됩니다.'); render(); return false; }
      if (result?.catch) result.catch(() => { if (pending.has(operation)) { item.status = 'uncertain'; say('전송 확인이 불확실합니다. 자동으로 다시 보내지 않습니다.'); render(); } });
    } catch {
      // A throwing transport may already have sent bytes: retain the operation fence.
      item.status = 'uncertain'; say('전송 확인이 불확실합니다. 자동으로 다시 보내지 않습니다.'); render(); return false;
    }
    say('접수 확인을 기다리는 중입니다.'); render(); return true;
  }
  function actionButton(caption, capability, intent, scope, className = 'cc-button') {
    const control = button(caption, () => emit(intent, scope), className);
    control.disabled = !connected || !enabled(capability) || isPending(scope);
    control.title = isPending(scope) ? '접수 확인을 기다리는 중입니다' : str(capability?.reason); return control;
  }
  function receipts(values) {
    for (const receipt of list(values).slice(-256)) {
      const item = pending.get(receipt.operation_id); if (!item) continue;
      if (receipt.status === 'uncertain') { item.status = 'uncertain'; say(str(receipt.detail) || '접수 여부가 불확실합니다. 자동으로 다시 보내지 않습니다.'); continue; }
      if (!['acknowledged', 'rejected'].includes(receipt.status)) continue;
      pending.delete(receipt.operation_id);
      if (receipt.status === 'acknowledged') {
        const draft = drafts.get(item.draftKey);
        if (item.draftKey && draft?.text === item.raw && draft.version === item.version) {
          draftTotal -= draft.size; drafts.delete(item.draftKey);
          if (currentKey === item.draftKey && input.value === item.raw) input.value = '';
        }
        if (item.settingsVersion !== undefined && settings.get(item.workspace)?.version === item.settingsVersion) settings.delete(item.workspace);
        if (item.planVersion !== undefined && plans.get(item.workspace)?.version === item.planVersion) plans.delete(item.workspace);
        if (item.resultKey && resultDrafts.get(item.resultKey)?.version === item.resultVersion) resultDrafts.delete(item.resultKey);
      }
      say(str(receipt.detail) || (receipt.status === 'acknowledged' ? '접수를 확인했습니다.' : '요청이 거절되었습니다. 초안은 보존됩니다.'));
    }
  }
  function updateComposer() {
    const p = project(), task = selectedTask();
    const target = screen === 'task' ? task?.target : screen === 'main' ? p?.main_target : null;
    const capability = screen === 'task' ? task?.input : p?.main_input;
    const nextKey = routeKey(target), caption = screen === 'task' ? str(task?.title) : label('main', '메인');
    if (currentKey !== nextKey) { currentKey = nextKey; input.value = drafts.get(nextKey)?.text || ''; }
    currentTarget = target ? copy(target) : null; currentCapability = capability;
    form.hidden = !target; input.disabled = !target || !connected || !enabled(capability);
    input.placeholder = input.disabled ? str(capability?.reason) || '현재 입력할 수 없습니다' : caption + '에게 목표나 변경할 내용을 알려 주세요';
    input.setAttribute('aria-label', caption + '에게 보낼 내용');
    targetCaption.textContent = caption + ' · ' + str(screen === 'task' ? task?.provider : p?.main_provider);
    const blocked = isPending('submit:' + nextKey), oversized = bytes(input.value.normalize('NFC')) > INPUT_LIMIT;
    submit.disabled = input.disabled || blocked || !input.value.trim() || oversized;
    submit.title = blocked ? '접수 여부를 확인할 때까지 다시 보낼 수 없습니다' : str(capability?.reason);
    submit.textContent = label('send', '보내기') + ' ↑';
    inputHint.textContent = blocked ? '접수 확인 대기 · 초안 보존' : oversized ? label('input_limit', '입력은 UTF-8 16 KiB까지 전송할 수 있습니다') : label('send_hint', 'Enter 전송 · Shift+Enter 줄바꿈');
  }
  input.addEventListener('input', () => { if (!rememberDraft()) input.value = drafts.get(currentKey)?.text || ''; updateComposer(); });
  input.addEventListener('compositionstart', () => { composing = true; });
  input.addEventListener('compositionend', () => { composing = false; compositionEndedAt = performance.now(); rememberDraft(); updateComposer(); });
  input.addEventListener('keydown', event => {
    if (event.key === 'Enter' && !event.shiftKey) {
      if (event.isComposing || composing || event.keyCode === 229) return;
      event.preventDefault(); if (performance.now() - compositionEndedAt < 50) return; form.requestSubmit();
    }
  });
  form.addEventListener('submit', event => {
    event.preventDefault();
    if (composing || input.disabled || submit.disabled || !currentTarget || !enabled(currentCapability) || !rememberDraft()) return;
    const raw = input.value, normalized = raw.normalize('NFC'); if (!normalized.trim() || bytes(normalized) > INPUT_LIMIT) return;
    emit({type: 'submit', target: currentTarget, text: normalized}, 'submit:' + currentKey, {draftKey: currentKey, raw, version: drafts.get(currentKey)?.version});
  });
  function navigate(next, selected = null) {
    if (!rememberDraft()) return; screen = next; taskId = selected; detailTab = 'work'; closeDrawers(); render(); content.scrollTop = 0;
  }
  function chooseProject(id) {
    if (!rememberDraft()) return; workspaceId = id; screen = 'main'; taskId = null; monitorFilter = 'all'; closeDrawers(); render(); content.scrollTop = 0;
    if (project()?.loaded === false) {
      if (readOnly) say('Mac에서 이 프로젝트를 열면 저장된 작업 화면이 표시됩니다.');
      else emit({type: 'load_project', workspace_id: id}, 'load:' + id);
    }
  }
  function closeDrawers() {
    drawer = null; app.classList.remove('cc-left-open', 'cc-right-open');
    for (const panel of [left, right]) { panel.removeAttribute('role'); panel.removeAttribute('aria-modal'); }
    if (drawerReturn?.isConnected) drawerReturn.focus(); drawerReturn = null;
  }
  function openDrawer(side, control) {
    closeDrawers(); drawer = side; drawerReturn = control;
    app.classList.add(side === 'left' ? 'cc-left-open' : 'cc-right-open');
    const panel = side === 'left' ? left : right;
    if (element.clientWidth <= (side === 'left' ? 760 : 1050)) { panel.setAttribute('role', 'dialog'); panel.setAttribute('aria-modal', 'true'); panel.querySelector('button:not(:disabled)')?.focus(); }
  }
  function terminalButton(task) {
    const id = str(task?.terminal_session_id);
    const remote = isRemoteTask(task);
    const control = button(label('terminal', '터미널'), () => {
      if (remote) { if (!rememberDraft()) return; screen = 'task'; taskId = task.target.task_id; detailTab = 'terminal'; closeDrawers(); requestRemoteView(task); render(); }
      else if (id && typeof watchSession === 'function') { setOpen(false); watchSession(id); }
    }, 'cc-button cc-ghost');
    control.dataset.ccTerminal = task?.target?.task_id || 'main';
    control.disabled = !connected || (!remote && (!id || typeof watchSession !== 'function')) || task?.terminal?.enabled === false;
    control.title = control.disabled ? str(task?.terminal?.reason) || '이 세션의 터미널 연결을 제공하지 않습니다' : '이 세션의 실제 터미널 열기'; return control;
  }
  function isRemoteTask(task) { return !!task?.target?.task_id && str(task?.target?.session_id).startsWith('remote:'); }
  function openCurrentTerminal() {
    if (screen === 'task') {
      const control = terminalButton(selectedTask());
      if (control.disabled) say(control.title); else control.click();
      return;
    }
    const p = project(), session = list(p?.sessions).find(s => s.target?.workspace_id === p.id && routeKey(s.target) === routeKey(p.main_target));
    const control = terminalButton(session);
    if (control.disabled) say(control.title); else control.click();
  }
  function openTaskDetail(task, tab = 'work') {
    if (!rememberDraft()) return;
    screen = 'task'; taskId = task.target?.task_id; detailTab = tab; closeDrawers(); render(); content.scrollTop = 0;
  }
  const needsAttention = task => ['waiting_approval', 'waiting_input', 'result_submitted', 'verification_failed', 'awaiting_user_confirmation', 'failed', 'observation_unknown'].includes(task.state);
  const hasResult = task => ['result_submitted', 'verification_failed', 'awaiting_user_confirmation', 'completed'].includes(task.state);
  function taskShortcut(task, className = 'cc-button') {
    const result = hasResult(task), caption = result ? '결과 확인' : task.state === 'waiting_approval' ? '승인 확인' : needsAttention(task) ? '상태 확인' : '작업 상세';
    const control = button(caption, () => openTaskDetail(task, result ? 'result' : 'work'), className);
    control.dataset.ccTaskAction = task.target?.task_id || '';
    control.dataset.ccDrawerFocus = 'action:' + routeKey(task.target);
    return control;
  }
  function remoteViewTask() { const task = selectedTask(); return screen === 'task' && detailTab === 'terminal' && isRemoteTask(task) && task.terminal?.enabled !== false ? task : null; }
  function requestRemoteView(task) { return emit({type: 'observe_remote_session', target: task.target}, 'observe:' + routeKey(task.target)); }
  function syncRemoteWatch() {
    if (!opened || !connected || destroyed || document.visibilityState === 'hidden' || !remoteViewTask()) { clearTimeout(remoteWatchTimer); remoteWatchTimer = null; return; }
    if (remoteWatchTimer !== null) return;
    remoteWatchTimer = setTimeout(() => {
      remoteWatchTimer = null;
      const task = remoteViewTask();
      if (opened && connected && !destroyed && document.visibilityState !== 'hidden' && task) requestRemoteView(task);
      syncRemoteWatch();
    }, 1000);
  }
  function renderRemoteTerminal(task) {
    const observed = task.remote_terminal, bar = node('div', 'cc-actions');
    bar.append(node('span', 'cc-muted', [observed?.status || '화면 관측을 기다립니다.', '읽기 전용'].join(' · ')));
    const refresh = button('화면 새로 고침', () => requestRemoteView(task), 'cc-button cc-ghost');
    refresh.disabled = !connected || isPending('observe:' + routeKey(task.target)); bar.append(refresh); content.append(bar);
    const surface = node('pre', 'cc-remote-terminal', str(observed?.text) || '원격 워커의 실제 화면을 조회합니다.');
    surface.dataset.ccRoute = routeKey(task.target); surface.tabIndex = 0; surface.setAttribute('aria-label', '원격 워커 터미널 · 읽기 전용'); content.append(surface);
    if (observed?.truncated) content.append(node('p', 'cc-muted', '전송 크기 제한으로 화면의 마지막 일부를 표시합니다.'));
  }
  function renderLeft() {
    left.replaceChildren(node('div', 'cc-side-label', label('workspaces', '워크스페이스')), button('닫기', closeDrawers, 'cc-button cc-ghost cc-drawer-close'));
    left.querySelector('.cc-drawer-close').dataset.ccDrawerFocus = 'close';
    for (const p of list(snapshot.projects).slice(0, 64)) {
      const row = button('', () => chooseProject(p.id), 'cc-project-row' + (p.id === workspaceId ? ' cc-active' : '')); row.dataset.ccProject = p.id; row.dataset.ccDrawerFocus = 'project:' + p.id;
      row.append(node('span', 'cc-project-glyph', '▱'), node('span', 'cc-grow', p.name), node('span', 'cc-muted', list(p.tasks).length));
      row.setAttribute('aria-current', p.id === workspaceId ? 'true' : 'false'); left.append(row);
      if (p.id === workspaceId) for (const session of list(p.sessions).slice(0, 64)) {
        const entry = button('', () => { closeDrawers(); if (session.terminal_session_id && typeof watchSession === 'function') { setOpen(false); watchSession(session.terminal_session_id); } else say('이 세션의 터미널 연결을 제공하지 않습니다.'); }, 'cc-session');
        entry.dataset.ccDrawerFocus = 'session:' + routeKey(session.target);
        entry.append(node('strong', '', session.title), node('small', '', [session.provider, session.model, session.effort, session.device].filter(Boolean).join(' · ')), stateTag(session.state, label)); left.append(entry);
      }
    }
    const foot = node('div', 'cc-side-foot'); foot.append(button(label('settings', '프로젝트 설정'), () => navigate('settings'), 'cc-button cc-ghost'), node('p', '', label('project_independent', '프로젝트·작업·입력은 각각 독립적입니다.'))); foot.querySelector('button').dataset.ccDrawerFocus = 'settings'; foot.querySelector('button').dataset.ccSettings = ''; left.append(foot);
  }
  function taskRow(task) {
    const row = button('', () => navigate('task', task.target?.task_id), 'cc-task-row'); row.dataset.ccTask = task.target?.task_id || '';
    const title = node('div', 'cc-task-title'); title.append(node('strong', '', task.title), node('small', '', task.scope));
    row.append(title, node('span', 'cc-owner', [task.provider, task.device].filter(Boolean).join(' · ')), stateTag(task.state, label));
    return row;
  }
  function renderRight(p) {
    const previousScroll = right.querySelector('.cc-monitor-scroll')?.scrollTop || 0;
    const tasks = list(p?.tasks), running = tasks.filter(t => t.state === 'running').length;
    const heading = node('header', 'cc-monitor-title'); heading.append(node('h2', '', label('workers', '작업자')), node('small', 'cc-muted', '실행 ' + running + (p?.config ? ' / 최대 ' + p.config.parallel_limit : '')), button(label('close', '닫기'), closeDrawers, 'cc-button cc-ghost cc-drawer-close')); right.replaceChildren(heading); heading.querySelector('button').dataset.ccDrawerFocus = 'close';
    const scroll = node('div', 'cc-monitor-scroll'), filters = node('div', 'cc-monitor-filter'); filters.setAttribute('aria-label', '작업자 필터');
    for (const [key, caption] of [['all', '전체'], ['attention', '확인 필요'], ['running', '실행 중']]) {
      const control = button(caption, () => { monitorFilter = key; render(); }, 'cc-button cc-ghost');
      control.dataset.ccMonitorFilter = key; control.dataset.ccDrawerFocus = 'filter:' + key; control.setAttribute('aria-pressed', String(monitorFilter === key)); filters.append(control);
    }
    scroll.append(filters);
    const visible = tasks.filter(task => monitorFilter === 'all' || monitorFilter === 'running' && task.state === 'running' || monitorFilter === 'attention' && needsAttention(task));
    if (!visible.length) scroll.append(node('p', 'cc-empty', tasks.length ? '해당 상태의 작업자가 없습니다.' : label('no_tasks', '배정된 작업자가 없습니다.')));
    for (const task of visible.slice(0, 64)) {
      const row = node('article', 'cc-worker-row' + (screen === 'task' && task.target?.task_id === taskId ? ' cc-selected' : ''));
      row.dataset.ccWorkerTask = task.target?.task_id || '';
      const top = node('div', 'cc-worker-heading'); top.append(node('span', 'cc-worker-role', [task.provider, task.role].filter(Boolean).join(' · ')), stateTag(task.state, label));
      const title = button(task.title, () => openTaskDetail(task), 'cc-worker-title'); title.dataset.ccTask = task.target?.task_id || ''; title.dataset.ccDrawerFocus = 'task:' + routeKey(task.target);
      const meta = node('div', 'cc-worker-meta'); meta.append(node('span', '', task.device || '—'), node('time', '', task.observed_at || '관측 시각 없음'));
      const actions = node('div', 'cc-worker-actions'), terminal = terminalButton(task); terminal.dataset.ccDrawerFocus = 'terminal:' + routeKey(task.target);
      terminal.classList.remove('cc-ghost'); terminal.append(' ↗'); actions.append(taskShortcut(task), terminal);
      row.append(top, title, node('small', 'cc-worker-scope', task.scope), node('p', 'cc-worker-activity', task.activity), meta, actions); scroll.append(row);
    }
    if (p) {
      const rules = node('section', 'cc-worker-rules'), config = p.config;
      rules.append(node('h3', '', '이 프로젝트의 실행 규칙'));
      if (config) rules.append(node('p', '', ['메인 ' + p.main_provider + ' → ' + list(config.worker_providers).join(' · '), config.use_worktrees ? '작업별 독립 worktree' : '공유 경로 · 수정 범위 확인', config.automation + ' · 최대 ' + config.parallel_limit + '개 병렬'].join('\n')));
      const settings = button('실행 규칙 설정 ↗', () => navigate('settings'), 'cc-button cc-ghost'); settings.dataset.ccSettings = ''; settings.dataset.ccDrawerFocus = 'settings'; rules.append(settings); scroll.append(rules);
    }
    const foot = node('footer', 'cc-monitor-foot'), devices = button('기기 관리 ↗', () => navigate('devices'), 'cc-button cc-ghost'); devices.dataset.ccDrawerFocus = 'devices';
    foot.append(node('small', 'cc-muted', '기기 ' + list(snapshot.devices).filter(device => device.connected).length + '대 연결'), devices); right.append(scroll, foot); scroll.scrollTop = previousScroll;
  }
  function title(caption, description) {
    const intro = node('div', 'cc-intro'); intro.append(node('h1', '', caption)); if (description) intro.append(node('p', 'cc-muted', description)); content.append(intro);
  }
  function renderHistory(entries, destination = content) {
    if (!entries.length) { destination.append(node('p', 'cc-empty', label('no_history', '아직 기록이 없습니다.'))); return; }
    for (const entry of entries.slice(-100).reverse()) {
      const row = node('div', 'cc-history-row'), body = node('div'); body.append(node('p', '', entry.text), node('small', 'cc-muted', entry.detail)); row.append(node('time', '', entry.time), body);
      if (entry.target?.workspace_id === workspaceId && entry.target?.task_id) row.append(button('작업 보기', () => navigate('task', entry.target.task_id), 'cc-button cc-ghost')); destination.append(row);
    }
  }
  function renderMain(p) {
    const tasks = list(p.tasks), goal = node('section', 'cc-goal');
    goal.append(node('p', 'cc-section-label', '프로젝트 목표 · ' + p.name), node('p', 'cc-instruction', p.goal || '이 프로젝트에서 어떤 작업을 진행할까요?'));
    const flow = node('div', 'cc-main-flow');
    for (const caption of ['목표', '계획·배정', '독립 실행 ' + tasks.filter(t => t.state === 'running').length, '결과 취합', '검증']) flow.append(node('span', '', caption));
    goal.append(flow); content.append(goal);
    const thread = (key, caption) => {
      const section = node('section', 'cc-thread-entry'), body = node('div', 'cc-thread-body'); section.dataset.ccThread = key;
      section.append(node('span', 'cc-thread-label', key.toUpperCase()), body); body.append(node('h2', '', caption)); content.append(section); return body;
    };
    const mainBody = thread('main', tasks.length ? '구현과 독립 리뷰를 나누고, 결과를 모아 검증합니다.' : '첫 목표를 메인 에이전트에게 맡겨 주세요.');
    mainBody.append(node('p', 'cc-main-summary', p.main_summary || (tasks.length ? '메인은 범위와 완료 조건을 정합니다. 각 작업자는 독립 세션에서 실행하고, 현재 진행은 오른쪽에서 계속 확인할 수 있습니다.' : '아래 입력창에 목표를 쓰면 배정안을 확인할 수 있습니다.')));
    const outline = node('div', 'cc-assignment-outline');
    tasks.slice(0, 3).forEach((task, index) => {
      const row = button('', () => openTaskDetail(task), 'cc-outline-row'); row.dataset.ccTask = task.target?.task_id || '';
      const body = node('span', 'cc-outline-title'), owner = node('span', 'cc-owner');
      body.append(node('strong', '', [task.role, task.title].filter(Boolean).join(' · ')), node('small', '', task.scope)); owner.append(node('span', '', task.provider), node('small', '', task.device));
      row.append(node('span', 'cc-outline-number', String(index + 1).padStart(2, '0')), body, owner); outline.append(row);
    }); mainBody.append(outline);
    if (tasks.length > 3) mainBody.append(button('전체 작업 ' + tasks.length + '개 보기 ↗', () => navigate('tasks'), 'cc-button cc-ghost'));
    if (p.config) {
      const foot = node('div', 'cc-main-summary-foot'); foot.append(node('span', '', p.config.use_worktrees ? '작업별 독립 worktree' : '공유 경로 · 수정 범위 확인'), node('span', '', '동시 실행 최대 ' + p.config.parallel_limit + ' · ' + p.config.automation)); mainBody.append(foot);
    }
    renderPlan(p, {manual: false});
    const attention = tasks.filter(needsAttention), approvals = readOnly ? [] : list(p.pending_approvals).filter(request => request.target?.workspace_id === p.id && request.approval && !tasks.some(task => routeKey(task.target) === routeKey(request.target) && task.approval?.id === request.approval.id));
    if (attention.length || approvals.length) {
      const checks = thread('check', '지금 확인할 작업 ' + (attention.length + approvals.length) + '건');
      checks.append(node('p', '', '명령 승인은 실행 전에, 결과 확인은 제출된 근거를 본 뒤 진행합니다.'));
      const actions = node('div', 'cc-check-actions');
      for (const task of attention) { const control = taskShortcut(task); control.append(node('span', '', ' · ' + [task.provider, task.role].filter(Boolean).join(' / '))); actions.append(control); } checks.append(actions);
      for (const request of approvals) {
        const approval = request.approval, box = node('section', 'cc-approval'); box.dataset.ccApproval = approval.id;
        box.append(node('h3', '', '실행 승인 요청 · ' + request.session_title), node('pre', 'cc-log', approval.command), facts([['폴더', approval.directory], ['범위', approval.scope]]));
        const actions = node('div', 'cc-actions');
        for (const allow of [false, true]) actions.append(actionButton(allow ? label('allow_once', '한 번 허용') : label('deny', '거절'), approval.action, {type: 'approval', target: request.target, approval_id: approval.id, allow}, 'approval:' + routeKey(request.target) + ':' + approval.id, allow ? 'cc-button cc-primary' : 'cc-button'));
        box.append(actions); checks.append(box);
      }
    }
    if (list(p.history).length) {
      const live = thread('live', '메인이 작업자 진행과 결과를 모읍니다.'); renderHistory(list(p.history).slice(-3), live);
      const all = button('프로젝트 전체 기록 ↗', () => navigate('history'), 'cc-button cc-ghost'); all.dataset.ccAllHistory = ''; live.append(all);
    }
    renderPlan(p, {review: false});
  }
  function renderPlan(p, {review = true, manual = true} = {}) {
    if (review && p.pending_goal_id) {
      const review = node('div', 'cc-review'); review.append(node('span', 'cc-grow', '작업 범위와 담당자를 검토한 뒤 계획을 확정해 주세요.'), actionButton(label('confirm_plan', '계획 확정'), p.plan_confirmation, {type: 'confirm_plan', workspace_id: p.id, goal_id: p.pending_goal_id}, 'plan:' + p.id)); content.append(review);
    }
    if (!manual || !p.manual_plan || !enabled(p.manual_plan) && !p.planner_prompt && !plans.has(p.id)) return;
    const details = node('details', 'cc-manual-plan'); details.append(node('summary', '', '계획 JSON 직접 검토·입력'), node('p', 'cc-muted', '메인 응답의 구조화된 계획을 붙여 넣으세요. 설치 후에도 계획 확정이 필요합니다.'));
    if (p.planner_prompt) { const prompt = node('details'); prompt.append(node('summary', '', '메인에게 전달할 계획 요청'), node('pre', 'cc-log', p.planner_prompt)); details.append(prompt); }
    const area = node('textarea', 'cc-plan-input'); area.dataset.ccFocus = 'plan-json'; area.setAttribute('aria-label', '작업 계획 JSON'); area.rows = 8; area.spellcheck = false; area.value = plans.get(p.id)?.text || ''; area.disabled = !enabled(p.manual_plan) || !connected;
    area.addEventListener('input', () => {
      const previous = plans.get(p.id); if (bytes(area.value) > PLAN_LIMIT) { area.value = previous?.text || ''; say('계획 JSON은 128 KiB까지 입력할 수 있습니다.'); return; }
      if (!previous && plans.size >= 64) { say('계획 초안 한도를 넘었습니다.'); return; }
      plans.set(p.id, {text: area.value, version: (previous?.version || 0) + 1});
    });
    const install = button('계획 검토에 올리기', () => {
      const plan = plans.get(p.id); if (!plan?.text.trim() || bytes(plan.text) > PLAN_LIMIT || !enabled(p.manual_plan)) return;
      try { JSON.parse(plan.text); } catch { say('유효한 JSON을 입력해 주세요.'); return; }
      emit({type: 'install_plan_json', workspace_id: p.id, json: plan.text}, 'install:' + p.id, {workspace: p.id, planVersion: plan.version});
    });
    install.disabled = !connected || !enabled(p.manual_plan) || isPending('install:' + p.id); install.title = str(p.manual_plan.reason); details.append(area, install); content.append(details);
  }
  function renderTask(p, task) {
    if (!task) { title(label('unavailable_task', '작업을 찾을 수 없습니다')); return; }
    title(task.title, [task.role, task.provider, task.device].filter(Boolean).join(' · '));
    const tabs = node('nav', 'cc-tabs'); tabs.setAttribute('aria-label', '작업 상세');
    for (const [key, caption] of [['work', '작업'], ['result', '결과'], ['git', 'Git'], ['history', '기록']]) {
      const tab = button(label(key, caption), () => { detailTab = key; render(); }, 'cc-tab' + (detailTab === key ? ' cc-active' : '')); tab.setAttribute('aria-current', detailTab === key ? 'page' : 'false'); tabs.append(tab);
    }
    tabs.append(terminalButton(task)); content.append(tabs);
    if (detailTab === 'terminal' && isRemoteTask(task)) { renderRemoteTerminal(task); return; }
    if (detailTab === 'history') { renderHistory(list(p.history).filter(h => h.target?.task_id === task.target?.task_id)); return; }
    if (detailTab === 'git') { content.append(node('p', 'cc-empty', str(task.git?.reason) || '이 원격 화면에서는 Git 변경 작업을 제공하지 않습니다.')); return; }
    if (detailTab === 'result') {
      content.append(node('pre', 'cc-log', task.result || label('no_result', '아직 제출된 결과가 없습니다.')));
      content.append(node('p', 'cc-muted', task.verification || '검증 미실행'));
      renderResultForms(task);
      if (task.result_revision) content.append(actionButton(label('confirm_result', '결과 확인'), task.confirm_result, {type: 'confirm_result', target: task.target, revision: task.result_revision}, 'result:' + routeKey(task.target))); return;
    }
    content.append(stateTag(task.state, label), facts([[label('scope', '범위'), task.scope], [label('criteria', '완료 기준'), task.acceptance_criteria], [label('directory', '실행 폴더'), task.directory], ['최근 관측', task.observed_at]]), node('p', 'cc-activity', task.activity));
    if (task.approval && !readOnly) {
      const approval = task.approval, box = node('section', 'cc-approval'); box.dataset.ccApproval = ''; box.append(node('h2', '', label('approval_title', '실행 승인 요청')), node('pre', 'cc-log', approval.command), facts([[label('directory', '폴더'), approval.directory], [label('scope', '범위'), approval.scope]]));
      const actions = node('div', 'cc-actions'); for (const allow of [false, true]) actions.append(actionButton(allow ? label('allow_once', '허용') : label('deny', '거절'), approval.action, {type: 'approval', target: task.target, approval_id: approval.id, allow}, 'approval:' + routeKey(task.target) + ':' + approval.id, allow ? 'cc-button cc-primary' : 'cc-button')); box.append(actions); content.append(box);
    }
    const actions = node('div', 'cc-actions'), selectionKey = routeKey({...task.target, session_id: null});
    const pool = Array.isArray(task.eligible_session_keys)
      ? list(p.worker_sessions).filter(s => task.eligible_session_keys.includes(s.target?.session_id))
      : list(task.eligible_sessions);
    const choices = pool.filter(s => s.target?.workspace_id === p.id && s.target?.session_id);
    const select = node('select'); select.setAttribute('aria-label', '작업 실행 세션'); select.dataset.ccWorker = '';
    const placeholder = node('option', '', '실행 세션 선택'); placeholder.value = ''; select.append(placeholder);
    for (const session of choices) { const option = node('option', '', session.title); option.value = session.target.session_id; select.append(option); }
    const chosen = workerSelections.get(selectionKey);
    select.value = choices.some(s => s.target.session_id === chosen) ? chosen : '';
    select.disabled = !connected || !enabled(task.dispatch);
    const dispatch = button(label('dispatch_task', '작업 실행'), () => {
      const selected = choices.find(s => s.target.session_id === workerSelections.get(selectionKey));
      if (!selected || !enabled(task.dispatch)) return;
      emit({type: 'dispatch_task', target: {...task.target, session_id: selected.target.session_id}}, 'dispatch:' + selectionKey);
    });
    const updateDispatch = () => { dispatch.disabled = !select.value || !connected || !enabled(task.dispatch) || isPending('dispatch:' + selectionKey); };
    select.addEventListener('change', () => { workerSelections.set(selectionKey, select.value); updateDispatch(); });
    dispatch.title = str(task.dispatch?.reason); updateDispatch(); actions.append(select, dispatch, terminalButton(task)); content.append(actions);
  }
  function renderResultForms(task) {
    if (!task.attempt_id || readOnly) return;
    const addForm = (kind, capability, caption, limit) => {
      if (!enabled(capability)) return;
      const key = JSON.stringify([routeKey(task.target), task.attempt_id, kind, kind === 'verify' ? task.result_revision : null]);
      let draft = resultDrafts.get(key);
      if (!draft) { if (resultDrafts.size >= 256) { content.append(node('p', 'cc-muted', '결과 초안 한도에 도달했습니다.')); return; } draft = {text: '', version: 0}; resultDrafts.set(key, draft); }
      const details = node('details', 'cc-manual-plan'); details.append(node('summary', '', caption));
      const area = node('textarea', 'cc-plan-input'); area.value = draft.text; area.dataset.ccFocus = kind + '-' + task.attempt_id; area.dataset.ccResult = kind; area.setAttribute('aria-label', caption); area.maxLength = limit;
      const scope = kind + ':' + key;
      const sendReport = passed => {
        if (!draft.text.trim() || bytes(draft.text.normalize('NFC')) > limit) { say(caption + '의 입력 크기를 확인해 주세요.'); return; }
        const intent = kind === 'report'
          ? {type: 'report_result', target: task.target, attempt_id: task.attempt_id, text: draft.text.normalize('NFC')}
          : {type: 'verify_result', target: task.target, attempt_id: task.attempt_id, revision: task.result_revision, passed, evidence: draft.text.normalize('NFC')};
        emit(intent, scope, {resultKey: key, resultVersion: draft.version});
      };
      area.addEventListener('input', () => { draft.text = area.value; draft.version++; });
      details.append(node('p', 'cc-muted', kind === 'report' ? '직접 확인한 결과를 기록합니다. 등록한 결과는 검증 대기 상태로 저장됩니다.' : '직접 수행한 검증의 명령과 결과를 기록해 주세요.'), area);
      for (const [text, passed] of kind === 'report' ? [['결과 등록', true]] : [['검증 실패 기록', false], ['검증 통과 기록', true]]) {
        const control = button(text, () => sendReport(passed)); control.disabled = !connected || isPending(scope); details.append(control);
      }
      content.append(details);
    };
    addForm('report', task.result_report, '수동 결과 보고', 8192);
    if (task.result_revision) addForm('verify', task.verify_result, '직접 수행한 검증 기록', 4096);
  }
  function renderSettings(p) {
    title('프로젝트 설정', '변경한 값은 저장 요청의 접수가 확인될 때까지 보존됩니다.');
    let draft = settings.get(p.id); if (!draft) { if (settings.size >= 64) { content.append(node('p', 'cc-empty', '설정 초안 한도를 넘었습니다.')); return; } draft = {config: copy(p.config), version: 0}; settings.set(p.id, draft); }
    else if (draft.version === 0) draft.config = copy(p.config);
    const fields = node('div', 'cc-form');
    function field(caption, key, choices = null, type = 'text') {
      const row = node('label', 'cc-field'); row.append(node('span', '', caption));
      const control = node(choices ? 'select' : type === 'paths' ? 'textarea' : 'input');
      if (choices) for (const [value, name] of choices) { const option = node('option', '', name); option.value = value; control.append(option); }
      else if (type !== 'paths') { control.type = type; if (type === 'text') control.maxLength = key === 'directory' ? 4096 : 1024; }
      if (type === 'paths') control.maxLength = 16384;
      if (type === 'checkbox') control.checked = !!draft.config[key]; else control.value = type === 'paths' ? list(draft.config[key]).join('\n') : draft.config[key] ?? '';
      if (key === 'parallel_limit') { control.min = '1'; control.max = '8'; }
      control.disabled = !enabled(p.save_settings) || !connected;
      control.dataset.ccFocus = 'setting-' + key;
      control.addEventListener(choices || type === 'checkbox' ? 'change' : 'input', () => { draft.config[key] = type === 'checkbox' ? control.checked : type === 'paths' ? control.value.split('\n').map(v => v.trim()).filter(Boolean) : key === 'parallel_limit' ? Number(control.value) : key === 'main_session_id' ? control.value || null : control.value; draft.version++; });
      row.append(control); fields.append(row);
    }
    field(label('name', '프로젝트 이름'), 'name'); field(label('directory', '프로젝트 폴더'), 'directory');
    const sessions = list(p.eligible_sessions ?? p.sessions);
    field(label('main_binding', '메인 세션'), 'main_session_id', [['', label('unbound', '연결하지 않음')], ...sessions.map(s => [s.target?.session_id || '', s.title])]);
    field(label('provider', '기본 제공자'), 'provider', list(p.available_providers).map(v => [v, v])); field(label('model', '모델'), 'model', list(p.available_models).map(v => [v, v])); field(label('effort', '추론 강도'), 'effort', list(p.available_efforts).map(v => [v, v]));
    field('허용 범위 (줄마다 상대 폴더)', 'allowed_scope_roots', null, 'paths');
    field(label('parallel_limit', '동시 작업 수'), 'parallel_limit', null, 'number'); field(label('automation', '자동화'), 'automation', list(p.available_automation).map(v => [v, v])); field(label('worktrees', '작업별 worktree 사용'), 'use_worktrees', null, 'checkbox');
    const providers = node('fieldset', 'cc-providers'); providers.append(node('legend', '', label('worker_providers', '작업자 제공자')));
    for (const provider of list(p.available_providers)) {
      const row = node('label'), control = node('input'); control.type = 'checkbox'; control.checked = list(draft.config.worker_providers).includes(provider); control.disabled = !enabled(p.save_settings) || !connected;
      control.addEventListener('change', () => { const chosen = new Set(draft.config.worker_providers); control.checked ? chosen.add(provider) : chosen.delete(provider); draft.config.worker_providers = [...chosen]; draft.version++; }); row.append(control, node('span', '', provider)); providers.append(row);
    }
    fields.append(providers);
    const save = button(label('save', '프로젝트 설정 저장'), () => {
      if (!enabled(p.save_settings)) return;
      if (!Number.isInteger(draft.config.parallel_limit) || draft.config.parallel_limit < 1 || draft.config.parallel_limit > 8 || !draft.config.name.trim() || !draft.config.directory.trim()) { say('이름·폴더와 동시 작업 수 1–8을 확인해 주세요.'); return; }
      emit({type: 'save_project', workspace_id: p.id, config: copy(draft.config)}, 'save:' + p.id, {workspace: p.id, settingsVersion: draft.version});
    }, 'cc-button cc-primary'); save.disabled = !enabled(p.save_settings) || !connected || isPending('save:' + p.id); save.title = str(p.save_settings?.reason); fields.append(save); content.append(fields);
  }
  function renderContent(p) {
    const scroll = content.scrollTop, active = content.contains(document.activeElement) ? document.activeElement : null;
    const focusKey = active?.dataset.ccFocus, selection = textSelection(active);
    const expanded = [...content.querySelectorAll('details')].map(detail => detail.open);
    content.replaceChildren();
    if (screen === 'devices') {
      title('연결된 기기', '열람·입력 권한과 작업 실행 권한은 각각 표시됩니다.');
      if (!list(snapshot.devices).length) content.append(node('p', 'cc-empty', '등록된 기기가 없습니다.'));
      for (const device of list(snapshot.devices).slice(0, 64)) {
        const row = node('div', 'cc-device-row'), body = node('div'); body.append(node('h2', '', device.name), node('small', 'cc-muted', device.description), node('p', 'cc-muted', str(device.capabilities))); row.append(body, node('span', device.connected ? 'cc-green' : 'cc-amber', device.connection), node('small', 'cc-muted', device.can_execute ? '작업 실행 가능' : label('viewer_only', '작업 실행 없음'))); content.append(row);
      }
    } else if (!p) title('프로젝트를 선택해 주세요', '호스트에서 등록한 프로젝트가 이곳에 표시됩니다.');
    else if (screen === 'task') renderTask(p, selectedTask());
    else if (screen === 'settings') renderSettings(p);
    else if (screen === 'history') { title('프로젝트 기록'); renderHistory(list(p.history)); }
    else if (screen === 'tasks') { title('작업', '현재 프로젝트에 배정된 독립 작업입니다.'); if (!list(p.tasks).length) content.append(node('p', 'cc-empty', '아직 배정된 작업이 없습니다.')); for (const task of list(p.tasks).slice(0, 64)) content.append(taskRow(task)); }
    else renderMain(p);
    content.scrollTop = scroll;
    [...content.querySelectorAll('details')].forEach((detail, index) => { detail.open = expanded[index] || false; });
    if (focusKey) {
      const replacement = [...content.querySelectorAll('[data-cc-focus]')].find(control => control.dataset.ccFocus === focusKey);
      replacement?.focus({preventScroll: true});
      if (selection && textSelection(replacement)) replacement.setSelectionRange(...selection);
    }
  }
  function rememberRemotePosition() {
    const surface = content.querySelector('.cc-remote-terminal');
    if (!opened || document.visibilityState === 'hidden' || !surface?.clientHeight || !surface.clientWidth) return;
    const key = surface.dataset.ccRoute;
    remotePositions.delete(key);
    remotePositions.set(key, {left: surface.scrollLeft, top: surface.scrollTop, following: surface.scrollTop + surface.clientHeight >= surface.scrollHeight - 2});
    if (remotePositions.size > 64) remotePositions.delete(remotePositions.keys().next().value);
  }
  function restoreRemotePosition() {
    const surface = content.querySelector('.cc-remote-terminal');
    if (!opened || !surface?.clientHeight || !surface.clientWidth) return;
    const position = remotePositions.get(surface.dataset.ccRoute);
    surface.scrollLeft = position?.left || 0;
    surface.scrollTop = position && !position.following ? position.top : surface.scrollHeight;
  }
  content.addEventListener('scroll', rememberRemotePosition, true);
  function render() {
    if (!snapshot || destroyed) return; applyTheme(); nav.replaceChildren();
    const activePanel = drawer === 'left' ? left : drawer === 'right' ? right : null;
    const drawerFocus = activePanel?.contains(document.activeElement) ? document.activeElement.dataset.ccDrawerFocus : null;
    rememberRemotePosition();
    app.classList.toggle('cc-monitor-collapsed', monitorCollapsed);
    for (const [key, fallback] of [['main', '메인 에이전트'], ['tasks', '작업'], ['terminal', '터미널'], ['devices', '에이전트·기기'], ['history', '이력']]) {
      const active = screen === key || screen === 'task' && key === (detailTab === 'terminal' ? 'terminal' : 'tasks');
      const tab = button(label('nav_' + key, fallback), () => key === 'terminal' ? openCurrentTerminal() : navigate(key), 'cc-nav-button' + (active ? ' cc-active' : '')); tab.dataset.ccNav = key; tab.setAttribute('aria-current', active ? 'page' : 'false'); nav.append(tab);
    }
    const p = project(); renderLeft(); renderRight(p); projectbar.replaceChildren();
    const heading = node('div', 'cc-grow'); heading.append(node('h2', '', p?.name || '프로젝트 없음'), node('small', 'cc-path', p?.directory || ''));
    const select = node('select', 'cc-project-select'); select.setAttribute('aria-label', '작업할 프로젝트'); for (const available of list(snapshot.projects).slice(0, 64)) { const option = node('option', '', available.name); option.value = available.id; select.append(option); } select.value = p?.id || ''; select.addEventListener('change', () => chooseProject(select.value));
    const settings = button('⚙', () => navigate('settings'), 'cc-button cc-ghost'); settings.dataset.ccSettings = ''; settings.setAttribute('aria-label', '프로젝트 설정'); projectbar.append(heading, select, settings);
    context.replaceChildren();
    const openLeft = button('☰', () => openDrawer('left', openLeft), 'cc-button cc-ghost cc-open-left'); openLeft.dataset.ccOpenLeft = ''; openLeft.setAttribute('aria-label', '프로젝트 목록 열기');
    const caption = node('div', 'cc-grow', screen === 'task' ? selectedTask()?.title || '작업' : screen === 'main' ? '메인 에이전트' : label(screen, {tasks: '프로젝트 작업', devices: '기기', history: '기록', settings: '설정'}[screen]));
    if (screen === 'main') caption.append(node('small', 'cc-context-model', [p?.main_provider, p?.main_model, p?.main_effort].filter(Boolean).join(' · ')));
    const openRight = button('작업자 ' + list(p?.tasks).length, () => openDrawer('right', openRight), 'cc-button cc-ghost cc-open-right'); openRight.dataset.ccOpenRight = ''; openRight.setAttribute('aria-label', '작업자 패널 열기');
    const toggle = button(monitorCollapsed ? '작업자 펼치기' : '작업자 접기', () => { monitorCollapsed = !monitorCollapsed; render(); }, 'cc-button cc-ghost cc-monitor-toggle'); toggle.dataset.ccMonitorToggle = ''; toggle.setAttribute('aria-label', monitorCollapsed ? '작업자 패널 펼치기' : '작업자 패널 접기'); toggle.setAttribute('aria-expanded', String(!monitorCollapsed));
    context.append(openLeft, caption); if (screen === 'task') context.append(button('메인으로', () => navigate('main'), 'cc-button cc-ghost')); context.append(openRight, toggle);
    renderContent(p); updateComposer(); status.textContent = connected ? str(snapshot.status) || '관측된 상태' : '연결 끊김 · 초안 보존 · 자동 재전송 없음';
    if (drawer) {
      drawerReturn = drawer === 'left' ? openLeft : openRight;
      const panel = drawer === 'left' ? left : right;
      if (panel.hasAttribute('aria-modal')) {
        const replacement = [...panel.querySelectorAll('[data-cc-drawer-focus]')].find(control => control.dataset.ccDrawerFocus === drawerFocus);
        (replacement && !replacement.disabled ? replacement : panel.querySelector('button:not(:disabled)'))?.focus({preventScroll: true});
      }
    }
    restoreRemotePosition();
    syncRemoteWatch();
  }
  function onKey(event) {
    if (event.key === 'Escape') { closeDrawers(); return; }
    if (event.key !== 'Tab' || !drawer) return;
    const panel = drawer === 'left' ? left : right; if (!panel.hasAttribute('aria-modal')) return;
    const controls = [...panel.querySelectorAll('button:not(:disabled),select:not(:disabled),input:not(:disabled),[tabindex="0"]')].filter(control => control.getClientRects().length), first = controls[0], last = controls.at(-1);
    if (event.shiftKey && (document.activeElement === first || !panel.contains(document.activeElement))) { event.preventDefault(); last?.focus(); }
    else if (!event.shiftKey && (document.activeElement === last || !panel.contains(document.activeElement))) { event.preventDefault(); first?.focus(); }
  }
  element.addEventListener('keydown', onKey);
  document.addEventListener('visibilitychange', syncRemoteWatch);
  const observer = typeof ResizeObserver === 'function' ? new ResizeObserver(() => { if (drawer) closeDrawers(); }) : null; observer?.observe(element);
  return {
    element,
    open() { setOpen(true); },
    accept(message) {
      if (destroyed) return false;
      if (message?.type === 'cloud_control_result') {
        if (!['invalid', 'rejected'].includes(message.status)) return false;
        const entry = [...pending.entries()].find(([, item]) => item.request === message.request);
        if (!entry) return false;
        pending.delete(entry[0]); say('호스트가 요청을 접수하지 않았습니다. 초안은 보존됩니다.'); render(); return true;
      }
      if (message?.type !== 'work_snapshot') return false;
      if (message.snapshot === null) { rememberDraft(); rememberRemotePosition(); snapshot = null; connected = false; for (const [id, item] of pending) if (item.observation) pending.delete(id); element.hidden = true; opened = false; notifyVisibility(false); syncRemoteWatch(); return true; }
      if (!message.snapshot || !Array.isArray(message.snapshot.projects)) return false;
      rememberDraft(); snapshot = message.snapshot; connected = true; receipts(snapshot.receipts);
      if (!list(snapshot.projects).some(p => p.id === workspaceId)) { workspaceId = snapshot.selected_workspace_id || list(snapshot.projects)[0]?.id || null; screen = 'main'; taskId = null; }
      element.hidden = false; if (!initialized) { initialized = true; opened = true; } app.hidden = !opened; reopen.hidden = opened; render(); notifyVisibility(opened); return true;
    },
    resetConnection() {
      if (destroyed) return; rememberDraft(); connected = false; for (const [id, item] of pending) { if (item.observation) pending.delete(id); else item.status = 'uncertain'; } say('연결이 끊겼습니다. 초안은 보존되며 요청을 자동으로 다시 보내지 않습니다.'); render();
    },
    destroy() {
      if (destroyed) return; destroyed = true; syncRemoteWatch(); observer?.disconnect(); element.removeEventListener('keydown', onKey); content.removeEventListener('scroll', rememberRemotePosition, true); document.removeEventListener('visibilitychange', syncRemoteWatch); pending.clear(); drafts.clear(); settings.clear(); plans.clear(); resultDrafts.clear(); workerSelections.clear(); remotePositions.clear(); element.remove(); notifyVisibility(false);
    },
  };
}
