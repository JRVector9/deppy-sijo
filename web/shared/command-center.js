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

export function createCommandCenterView({host, send, watchSession, onVisibility, readOnly = false} = {}) {
  if (!host?.append) throw new TypeError('Command Center host element required');
  const enabled = capability => !readOnly && capability?.enabled === true;
  let snapshot = null, workspaceId = null, screen = 'main', taskId = null, detailTab = 'work';
  let connected = false, opened = false, initialized = false, destroyed = false, composing = false;
  let currentKey = '', currentTarget = null, currentCapability = null, draftTotal = 0, draftSerial = 0, compositionEndedAt = -Infinity;
  let themeOverride = null, drawer = null, drawerReturn = null, lastVisibility = null;
  const drafts = new Map(), pending = new Map(), settings = new Map(), plans = new Map(), resultDrafts = new Map(), workerSelections = new Map();
  const element = node('section', 'cc-shell'); element.hidden = true;
  const reopen = button('지휘 터미널 열기', () => setOpen(true), 'cc-button cc-reopen'); reopen.dataset.ccOpen = '';
  const app = node('section', 'cc-app'); app.setAttribute('aria-label', '에이전트 지휘 터미널');
  const header = node('header', 'cc-header'), brand = node('div', 'cc-brand');
  brand.append(node('span', 'cc-mark', 'D'), node('span', 'cc-brand-name', 'Deppy Sijo'));
  const nav = node('nav', 'cc-nav'); nav.setAttribute('aria-label', '주요 화면');
  const headActions = node('div', 'cc-head-actions');
  const theme = button('라이트', () => { themeOverride = !app.classList.contains('cc-light'); applyTheme(); }, 'cc-button cc-ghost');
  headActions.append(theme, button('터미널로', () => setOpen(false), 'cc-button cc-ghost'));
  header.append(brand, nav, headActions);
  const layout = node('div', 'cc-layout');
  const backdrop = button('패널 닫기', closeDrawers, 'cc-backdrop'); backdrop.tabIndex = -1;
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
    opened = value; app.hidden = !value; reopen.hidden = value;
    if (!value) closeDrawers(); notifyVisibility(value);
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
    if (!connected || destroyed || isPending(scope)) return false;
    if (pending.size >= 16) { say('접수 확인 대기 항목이 많습니다. 기존 요청을 확인한 뒤 다시 시도해 주세요.'); return false; }
    if (nextRequest > 0xffffffff) { say('요청 번호를 모두 사용했습니다. 연결을 새로 열어 주세요.'); return false; }
    if (!globalThis.crypto?.randomUUID) { say('보안 연결에서만 요청을 전송할 수 있습니다.'); return false; }
    const operation = crypto.randomUUID(), item = {scope, request: nextRequest++, status: 'pending', ...metadata};
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
    if (!rememberDraft()) return; workspaceId = id; screen = 'main'; taskId = null; closeDrawers(); render(); content.scrollTop = 0;
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
    const control = button(label('terminal', '터미널'), () => { if (id && typeof watchSession === 'function') { setOpen(false); watchSession(id); } }, 'cc-button cc-ghost');
    control.dataset.ccTerminal = task?.target?.task_id || 'main';
    control.disabled = !id || typeof watchSession !== 'function' || (task?.terminal && !enabled(task.terminal));
    control.title = control.disabled ? str(task?.terminal?.reason) || '이 세션의 터미널 연결을 제공하지 않습니다' : '이 세션의 실제 터미널 열기'; return control;
  }
  function renderLeft() {
    left.replaceChildren(node('div', 'cc-side-label', label('workspaces', '워크스페이스')), button('닫기', closeDrawers, 'cc-button cc-ghost cc-drawer-close'));
    for (const p of list(snapshot.projects).slice(0, 64)) {
      const row = button('', () => chooseProject(p.id), 'cc-project-row' + (p.id === workspaceId ? ' cc-active' : '')); row.dataset.ccProject = p.id;
      row.append(node('span', 'cc-project-glyph', '▱'), node('span', 'cc-grow', p.name), node('span', 'cc-muted', list(p.tasks).length));
      row.setAttribute('aria-current', p.id === workspaceId ? 'true' : 'false'); left.append(row);
      if (p.id === workspaceId) for (const session of list(p.sessions).slice(0, 64)) {
        const entry = button('', () => { closeDrawers(); if (session.terminal_session_id && typeof watchSession === 'function') { setOpen(false); watchSession(session.terminal_session_id); } else say('이 세션의 터미널 연결을 제공하지 않습니다.'); }, 'cc-session');
        entry.append(node('strong', '', session.title), node('small', '', [session.provider, session.model, session.effort, session.device].filter(Boolean).join(' · ')), stateTag(session.state, label)); left.append(entry);
      }
    }
    const foot = node('div', 'cc-side-foot'); foot.append(button(label('settings', '프로젝트 설정'), () => navigate('settings'), 'cc-button cc-ghost'), node('p', '', label('project_independent', '프로젝트·작업·입력은 각각 독립적입니다.'))); left.append(foot);
  }
  function taskRow(task, compact = false) {
    const row = button('', () => navigate('task', task.target?.task_id), compact ? 'cc-worker-row' : 'cc-task-row'); row.dataset.ccTask = task.target?.task_id || '';
    const title = node('div', 'cc-task-title'); title.append(node('strong', '', task.title), node('small', '', compact ? task.activity : task.scope));
    row.append(title, node('span', 'cc-owner', [task.provider, task.device].filter(Boolean).join(' · ')), stateTag(task.state, label));
    if (compact) row.append(node('small', 'cc-worker-time', task.observed_at || '관측 시각 없음')); return row;
  }
  function renderRight(p) {
    const heading = node('div', 'cc-monitor-title'); heading.append(node('h2', '', label('workers', '작업자') + ' ' + list(p?.tasks).length), button(label('close', '닫기'), closeDrawers, 'cc-button cc-ghost cc-drawer-close')); right.replaceChildren(heading);
    if (!list(p?.tasks).length) right.append(node('p', 'cc-empty', label('no_tasks', '배정된 작업자가 없습니다.')));
    for (const task of list(p?.tasks).slice(0, 64)) right.append(taskRow(task, true));
    right.append(node('p', 'cc-monitor-note', '관측된 실행·승인·결과 상태를 표시합니다. 결과 제출과 검증 완료는 구분됩니다.'));
  }
  function title(caption, description) {
    const intro = node('div', 'cc-intro'); intro.append(node('h1', '', caption)); if (description) intro.append(node('p', 'cc-muted', description)); content.append(intro);
  }
  function renderHistory(entries) {
    if (!entries.length) { content.append(node('p', 'cc-empty', label('no_history', '아직 기록이 없습니다.'))); return; }
    for (const entry of entries.slice(-100).reverse()) {
      const row = node('div', 'cc-history-row'), body = node('div'); body.append(node('p', '', entry.text), node('small', 'cc-muted', entry.detail)); row.append(node('time', '', entry.time), body);
      if (entry.target?.task_id) row.append(button('작업 보기', () => navigate('task', entry.target.task_id), 'cc-button cc-ghost')); content.append(row);
    }
  }
  function renderPlan(p) {
    if (p.pending_goal_id) {
      const review = node('div', 'cc-review'); review.append(node('span', 'cc-grow', '작업 범위와 담당자를 검토한 뒤 계획을 확정해 주세요.'), actionButton(label('confirm_plan', '계획 확정'), p.plan_confirmation, {type: 'confirm_plan', workspace_id: p.id, goal_id: p.pending_goal_id}, 'plan:' + p.id)); content.append(review);
    }
    if (!p.manual_plan) return;
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
    const focusKey = active?.dataset.ccFocus, selection = active && 'selectionStart' in active ? [active.selectionStart, active.selectionEnd] : null;
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
    else {
      title('메인에게 작업 맡기기', [p.main_provider, p.main_model, p.main_effort].filter(Boolean).join(' · '));
      const goal = node('section', 'cc-goal'); goal.append(node('p', 'cc-section-label', '현재 목표'), node('p', 'cc-instruction', p.goal || '아직 제출한 목표가 없습니다.')); content.append(goal);
      if (p.main_summary) { const summary = node('section', 'cc-main-summary'); summary.append(node('h2', '', '메인'), node('p', '', p.main_summary)); content.append(summary); }
      const tasks = node('section', 'cc-task-table'); tasks.append(node('h2', 'cc-section-label', '배정된 작업')); if (!list(p.tasks).length) tasks.append(node('p', 'cc-empty', '메인과 목표를 논의한 뒤 검토할 계획을 등록해 주세요.')); for (const task of list(p.tasks).slice(0, 64)) tasks.append(taskRow(task)); content.append(tasks); renderPlan(p);
    }
    content.scrollTop = scroll;
    [...content.querySelectorAll('details')].forEach((detail, index) => { detail.open = expanded[index] || false; });
    if (focusKey) {
      const replacement = [...content.querySelectorAll('[data-cc-focus]')].find(control => control.dataset.ccFocus === focusKey);
      replacement?.focus({preventScroll: true});
      if (replacement && selection && replacement.type !== 'number') replacement.setSelectionRange?.(...selection);
    }
  }
  function render() {
    if (!snapshot || destroyed) return; applyTheme(); nav.replaceChildren();
    for (const [key, fallback] of [['main', '메인'], ['tasks', '작업'], ['devices', '기기'], ['history', '기록'], ['settings', '설정']]) {
      const tab = button(label(key, fallback), () => navigate(key), 'cc-nav-button' + ((screen === key || (screen === 'task' && key === 'tasks')) ? ' cc-active' : '')); tab.dataset.ccNav = key; tab.setAttribute('aria-current', screen === key ? 'page' : 'false'); nav.append(tab);
    }
    const p = project(); renderLeft(); renderRight(p); projectbar.replaceChildren();
    const heading = node('div', 'cc-grow'); heading.append(node('h2', '', p?.name || '프로젝트 없음'), node('small', 'cc-path', p?.directory || ''));
    const select = node('select', 'cc-project-select'); select.setAttribute('aria-label', '작업할 프로젝트'); for (const available of list(snapshot.projects).slice(0, 64)) { const option = node('option', '', available.name); option.value = available.id; select.append(option); } select.value = p?.id || ''; select.addEventListener('change', () => chooseProject(select.value)); projectbar.append(heading, select);
    context.replaceChildren();
    const openLeft = button('☰', () => openDrawer('left', openLeft), 'cc-button cc-ghost cc-open-left'); openLeft.dataset.ccOpenLeft = ''; openLeft.setAttribute('aria-label', '프로젝트 목록 열기');
    const caption = node('div', 'cc-grow', screen === 'task' ? selectedTask()?.title || '작업' : screen === 'main' ? '메인 지휘' : label(screen, {tasks: '프로젝트 작업', devices: '기기', history: '기록', settings: '설정'}[screen]));
    const openRight = button('작업자 ' + list(p?.tasks).length, () => openDrawer('right', openRight), 'cc-button cc-ghost cc-open-right'); openRight.dataset.ccOpenRight = ''; openRight.setAttribute('aria-label', '작업자 패널 열기');
    context.append(openLeft, caption); if (screen === 'task') context.append(button('메인으로', () => navigate('main'), 'cc-button cc-ghost')); context.append(openRight);
    renderContent(p); updateComposer(); status.textContent = connected ? str(snapshot.status) || '관측된 상태' : '연결 끊김 · 초안 보존 · 자동 재전송 없음';
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
      if (message.snapshot === null) { rememberDraft(); snapshot = null; connected = false; element.hidden = true; opened = false; notifyVisibility(false); return true; }
      if (!message.snapshot || !Array.isArray(message.snapshot.projects)) return false;
      rememberDraft(); snapshot = message.snapshot; connected = true; receipts(snapshot.receipts);
      if (!list(snapshot.projects).some(p => p.id === workspaceId)) { workspaceId = snapshot.selected_workspace_id || list(snapshot.projects)[0]?.id || null; screen = 'main'; taskId = null; }
      element.hidden = false; if (!initialized) { initialized = true; opened = true; } app.hidden = !opened; reopen.hidden = opened; render(); notifyVisibility(opened); return true;
    },
    resetConnection() {
      if (destroyed) return; rememberDraft(); connected = false; for (const item of pending.values()) item.status = 'uncertain'; say('연결이 끊겼습니다. 초안은 보존되며 요청을 자동으로 다시 보내지 않습니다.'); render();
    },
    destroy() {
      if (destroyed) return; destroyed = true; observer?.disconnect(); element.removeEventListener('keydown', onKey); pending.clear(); drafts.clear(); settings.clear(); plans.clear(); resultDrafts.clear(); workerSelections.clear(); element.remove(); notifyVisibility(false);
    },
  };
}
