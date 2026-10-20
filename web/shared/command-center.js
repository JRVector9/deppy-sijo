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
function icon(name) {
  const svg = document.createElementNS('http://www.w3.org/2000/svg', 'svg');
  svg.setAttribute('viewBox', '0 0 24 24'); svg.setAttribute('aria-hidden', 'true'); svg.classList.add('cc-icon');
  const path = document.createElementNS(svg.namespaceURI, 'path');
  path.setAttribute('d', {send: 'M12 19V5m-6 6 6-6 6 6', terminal: 'm5 7 5 5-5 5m8 0h6', monitor: 'M4 4h16v16H4zM14 4v16', menu: 'M4 6h16M4 12h16M4 18h16', device: 'M4 4h16v12H4zM8 20h8m-4-4v4'}[name] || 'M4 4h16v16H4z');
  svg.append(path); return svg;
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
  let remoteWatchTimer = null, agentPicker = null, contentIdentity = '';
  const drafts = new Map(), pending = new Map(), settings = new Map(), plans = new Map(), resultDrafts = new Map(), workerSelections = new Map(), remotePositions = new Map(), agentDrafts = new Map();
  const element = node('section', 'cc-shell'); element.hidden = true;
  const reopen = button('지휘 터미널 열기', () => setOpen(true), 'cc-button cc-reopen'); reopen.dataset.ccOpen = '';
  const app = node('section', 'cc-app'); app.setAttribute('aria-label', '에이전트 지휘 터미널');
  const header = node('header', 'cc-header'), brand = node('div', 'cc-brand');
  brand.append(node('span', 'cc-mark', 'D'), node('span', 'cc-brand-name', 'Deppy Sijo'));
  const nav = node('nav', 'cc-nav'); nav.setAttribute('aria-label', '주요 화면');
  const headActions = node('div', 'cc-head-actions');
  const theme = button('라이트', () => { themeOverride = !app.classList.contains('cc-light'); applyTheme(); }, 'cc-button cc-ghost');
  const close = button('×', () => setOpen(false), 'cc-button cc-ghost'); close.dataset.ccClose = ''; close.setAttribute('aria-label', '지휘 화면 접기');
  const deviceCount = node('span', 'cc-device-count'); headActions.append(deviceCount, theme, close);
  header.append(brand, nav, headActions);
  const layout = node('div', 'cc-layout');
  const backdrop = button('', closeDrawers, 'cc-backdrop'); backdrop.tabIndex = -1; backdrop.setAttribute('aria-label', '패널 닫기');
  const left = node('aside', 'cc-sidebar'); left.setAttribute('aria-label', '프로젝트와 독립 세션');
  const workspace = node('div', 'cc-workspace'), projectbar = node('header', 'cc-projectbar');
  const workbody = node('div', 'cc-workbody'), main = node('main', 'cc-work');
  const context = node('div', 'cc-context'), content = node('div', 'cc-content'); content.tabIndex = -1;
  const form = node('form', 'cc-composer-wrap'), composer = node('div', 'cc-composer');
  const input = node('textarea', 'cc-input'); input.rows = 2; input.dataset.ccComposer = '';
  const composerFoot = node('div', 'cc-composer-foot'), targetCaption = button('', () => openAgentPicker(), 'cc-target'), inputHint = node('span', 'cc-input-hint'); targetCaption.dataset.ccTarget = '';
  const submit = node('button', 'cc-button cc-primary cc-send'); submit.append(icon('send')); submit.type = 'submit'; submit.dataset.ccSend = '';
  composerFoot.append(targetCaption, inputHint, submit); composer.append(input, composerFoot); form.append(composer);
  main.append(context, content, form);
  const right = node('aside', 'cc-monitor'); right.setAttribute('aria-label', '현재 프로젝트 작업자 모니터');
  workbody.append(main, right); workspace.append(projectbar, workbody); layout.append(backdrop, left, workspace);
  const footer = node('footer', 'cc-status'), status = node('span', 'cc-status-text'); footer.append(node('span', 'cc-grow cc-accent', '메인 → 독립 작업자 → 결과 취합 → 검증'), status);
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
        if (item.agentVersion !== undefined) {
          const draft = agentDrafts.get(item.workspace), saved = list(snapshot.projects).find(p => p.id === item.workspace)?.config;
          if (draft && saved) { if (draft.version === item.agentVersion) draft.config = copy(saved); else draft.config.revision = saved.revision; }
        }
        if (item.planVersion !== undefined && plans.get(item.workspace)?.version === item.planVersion) plans.delete(item.workspace);
        if (item.resultKey && resultDrafts.get(item.resultKey)?.version === item.resultVersion) resultDrafts.delete(item.resultKey);
      }
      say(str(receipt.detail) || (receipt.status === 'acknowledged' ? '접수를 확인했습니다.' : '요청이 거절되었습니다. 초안은 보존됩니다.'));
    }
  }
  function updateComposer() {
    const p = project(), task = selectedTask();
    const candidate = screen === 'task' ? task?.target : screen === 'main' ? p?.main_target : null;
    const target = candidate?.workspace_id === p?.id && (screen !== 'main' || !candidate?.task_id) ? candidate : null;
    const capability = screen === 'task' ? task?.input : p?.main_input;
    // A project's main draft belongs to its main role even before a session is bound.
    // Task drafts retain the exact opaque session/task owner key.
    const nextKey = screen === 'main' && p ? routeKey({workspace_id: p.id, session_id: null, task_id: null}) : routeKey(target);
    const bound = !!target?.session_id && (screen !== 'main' || p?.config?.main_session_id === target.session_id);
    const caption = screen === 'task' ? [task?.provider, task?.role, task?.device].filter(Boolean).join(' · ') : p?.name + ' / ' + (bound ? '메인 ' + p.main_provider : '메인 에이전트 선택');
    if (currentKey !== nextKey) { currentKey = nextKey; input.value = drafts.get(nextKey)?.text || ''; }
    currentTarget = target ? copy(target) : null; currentCapability = capability;
    form.hidden = !(screen === 'main' && p || screen === 'task' && task);
    input.disabled = readOnly || !nextKey || isRemoteTask(task);
    input.placeholder = input.disabled ? str(capability?.reason) || '읽기 전용 화면입니다' : screen === 'task' ? '이 에이전트에게 추가 내용을 알려 주세요' : '메인에게 목표를 알려 주세요 · 구현·리뷰·검증으로 나눕니다';
    input.setAttribute('aria-label', caption + '에게 보낼 내용');
    targetCaption.textContent = caption; targetCaption.disabled = readOnly || screen !== 'main'; targetCaption.setAttribute('aria-expanded', String(agentPicker === p?.id));
    targetCaption.title = screen === 'main' ? '메인 에이전트와 실행 설정 선택' : '선택한 작업의 정확한 담당 세션';
    const blocked = isPending('submit:' + nextKey), oversized = bytes(input.value.normalize('NFC')) > INPUT_LIMIT;
    submit.disabled = input.disabled || !connected || blocked || !input.value.trim() || oversized || bound && !enabled(capability);
    submit.title = blocked ? '접수 여부를 확인할 때까지 다시 보낼 수 없습니다' : str(capability?.reason); submit.setAttribute('aria-label', caption + '에게 전송');
    inputHint.textContent = blocked ? '접수 확인 대기 · 초안 보존' : oversized ? label('input_limit', '입력은 UTF-8 16 KiB까지 전송할 수 있습니다') : label('send_hint', 'Enter 전송 · Shift+Enter 줄바꿈');
    renderAgentPicker();
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
    if (composing || input.disabled || !rememberDraft() || !input.value.trim()) return;
    if (screen === 'main' && (!currentTarget?.session_id || project()?.config?.main_session_id !== currentTarget.session_id)) { openAgentPicker(); return; }
    if (submit.disabled || !connected || !currentTarget || !enabled(currentCapability)) return;
    const raw = input.value, normalized = raw.normalize('NFC'); if (!normalized.trim() || bytes(normalized) > INPUT_LIMIT) return;
    emit({type: 'submit', target: currentTarget, text: normalized}, 'submit:' + currentKey, {draftKey: currentKey, raw, version: drafts.get(currentKey)?.version});
  });
  function openAgentPicker() {
    const p = project(); if (readOnly || screen !== 'main' || !p || !rememberDraft()) return;
    if (!agentDrafts.has(p.id)) {
      if (agentDrafts.size >= 64) { say('설정 초안 한도를 넘었습니다.'); return; }
      agentDrafts.set(p.id, {config: copy(p.config), version: 0});
    }
    agentPicker = p.id; renderAgentPicker(); targetCaption.setAttribute('aria-expanded', 'true');
    form.querySelector('[data-cc-agent-session]')?.focus({preventScroll: true});
  }
  function closeAgentPicker(discard = false) {
    if (discard) agentDrafts.delete(agentPicker); agentPicker = null; renderAgentPicker(); targetCaption.setAttribute('aria-expanded', 'false');
  }
  function agentChoices(p) {
    const sessions = list(p.eligible_sessions ?? p.sessions).filter(s => s.target?.workspace_id === p.id && s.target?.session_id && !s.target?.task_id);
    const agents = list(p.available_agents).filter(a => str(a.provider)).map(a => ({...a, model: str(a.model), effort: str(a.effort)}));
    // Older snapshots still expose observed tuples. Never manufacture provider/model pairs.
    const observed = [...sessions]; if (!agents.length && p.config?.main_session_id) observed.push(p.config);
    for (const a of observed) if (str(a.provider) && a.provider !== 'unbound' && !agents.some(v => v.provider === a.provider && v.model === str(a.model) && str(v.effort) === str(a.effort))) agents.push({provider: a.provider, model: str(a.model), effort: str(a.effort)});
    return {sessions, agents};
  }
  function renderAgentPicker() {
    const old = form.querySelector('[data-cc-agent-picker]'), focus = old?.contains(document.activeElement) ? document.activeElement : null;
    const focusKey = focus?.dataset.ccFocus, selection = textSelection(focus), scroll = old?.scrollTop || 0;
    old?.remove(); const p = project(), draft = agentDrafts.get(agentPicker);
    if (readOnly || screen !== 'main' || agentPicker !== p?.id || !draft) return;
    const panel = node('section', 'cc-agent-picker'); panel.dataset.ccAgentPicker = ''; panel.setAttribute('role', 'dialog'); panel.setAttribute('aria-label', '메인 에이전트 선택');
    const heading = node('div', 'cc-picker-heading'); heading.append(node('h2', '', '메인 에이전트 선택'), button('×', () => { closeAgentPicker(); targetCaption.focus({preventScroll: true}); }, 'cc-button cc-ghost')); panel.append(heading);
    panel.append(node('p', 'cc-muted', '연결과 설정을 확인한 뒤 작성한 내용을 다시 전송해 주세요.'));
    const {sessions, agents} = agentChoices(p);
    const config = draft.config;
    const selectField = (caption, key, values, placeholder, change) => {
      const row = node('label', 'cc-field'), select = node('select'); row.append(node('span', '', caption));
      const empty = node('option', '', placeholder); empty.value = ''; select.append(empty);
      for (const [value, caption] of values) { const option = node('option', '', caption); option.value = value; select.append(option); }
      select.value = config[key] || ''; select.dataset['ccAgent' + ({main_session_id: 'Session', provider: 'Provider', model: 'Model', effort: 'Effort'}[key])] = ''; select.dataset.ccFocus = 'agent-' + key;
      select.addEventListener('change', () => { change(select.value); draft.version++; renderAgentPicker(); }); row.append(select); return row;
    };
    panel.append(selectField('기존 메인 세션', 'main_session_id', sessions.map(s => [s.target.session_id, [s.title, s.provider, s.model, s.effort, s.device].filter(Boolean).join(' · ')]), '연결할 세션 선택', value => {
      const selected = sessions.find(s => s.target.session_id === value); config.main_session_id = value || null;
      if (selected) for (const key of ['provider', 'model', 'effort']) config[key] = str(selected[key]);
    }));
    const inline = node('div', 'cc-inline-fields');
    inline.append(selectField('에이전트', 'provider', [...new Set(agents.map(a => a.provider))].map(v => [v, v]), '에이전트 선택', value => { config.provider = value; config.model = ''; config.effort = ''; config.main_session_id = null; }));
    const models = [...new Set(agents.filter(a => a.provider === config.provider).map(a => a.model))];
    inline.append(selectField('모델', 'model', models.filter(Boolean).map(v => [v, v]), models.includes('') ? '기본 모델' : '모델 선택', value => { config.model = value; config.effort = ''; config.main_session_id = null; })); panel.append(inline);
    const efforts = [...new Set(agents.filter(a => a.provider === config.provider && a.model === config.model).map(a => str(a.effort)))];
    panel.append(selectField('추론 강도', 'effort', efforts.filter(Boolean).map(v => [v, v]), efforts.includes('') ? '기본 설정 유지' : '추론 강도 선택', value => { config.effort = value; config.main_session_id = null; }));
    const workers = node('fieldset', 'cc-picker-workers'); workers.append(node('legend', '', '작업 에이전트 · 하나 이상 선택'));
    for (const provider of [...new Set([...agents.map(a => a.provider), ...list(config.worker_providers)])]) {
      const row = node('label'), control = node('input'); control.type = 'checkbox'; control.checked = list(config.worker_providers).includes(provider); control.dataset.ccAgentWorker = provider; control.dataset.ccFocus = 'agent-worker-' + provider;
      control.addEventListener('change', () => { const chosen = new Set(config.worker_providers); control.checked ? chosen.add(provider) : chosen.delete(provider); config.worker_providers = [...chosen]; draft.version++; renderAgentPicker(); }); row.append(control, node('span', '', provider)); workers.append(row);
    }
    panel.append(workers);
    if (!list(config.allowed_scope_roots).length || draft.scopesEdited) {
      const row = node('label', 'cc-field'), area = node('textarea'); row.append(node('span', '', '작업 범위 · 줄마다 상대 폴더')); area.rows = 2; area.value = list(config.allowed_scope_roots).join('\n'); area.dataset.ccFocus = 'agent-scopes'; area.dataset.ccAgentScopes = '';
      area.addEventListener('input', () => { config.allowed_scope_roots = area.value.split('\n').map(v => v.trim()).filter(Boolean); draft.scopesEdited = true; draft.version++; renderAgentPicker(); }); row.append(area); panel.append(row);
    }
    const selectedSession = sessions.find(s => s.target.session_id === config.main_session_id);
    const tupleValid = config.main_session_id ? !!selectedSession && ['provider', 'model', 'effort'].every(key => str(selectedSession[key]) === str(config[key])) : agents.some(a => a.provider === config.provider && a.model === config.model && str(a.effort) === str(config.effort));
    const valid = tupleValid && list(config.worker_providers).length > 0 && list(config.allowed_scope_roots).length > 0;
    const comparable = value => JSON.stringify(Object.fromEntries(Object.entries(value).filter(([key]) => key !== 'revision').sort(([a], [b]) => a.localeCompare(b))));
    const saved = comparable(config) === comparable(p.config), actions = node('div', 'cc-actions');
    const save = button('설정 저장', () => {
      if (!valid || !enabled(p.save_settings) || !connected || isPending('save:' + p.id)) return;
      emit({type: 'save_project', workspace_id: p.id, config: copy(config)}, 'save:' + p.id, {workspace: p.id, agentVersion: draft.version});
    }, 'cc-button cc-primary'); save.dataset.ccAgentSave = ''; save.disabled = !valid || !connected || !enabled(p.save_settings) || isPending('save:' + p.id); save.title = str(p.save_settings?.reason);
    const create = button('새 메인 만들기', () => {
      if (!saved || config.main_session_id || !valid || !connected || !enabled(p.create_main)) return;
      emit({type: 'create_main', workspace_id: p.id}, 'create-main:' + p.id);
    }); create.dataset.ccAgentCreate = ''; create.disabled = !saved || !!config.main_session_id || !valid || !connected || !enabled(p.create_main) || isPending('save:' + p.id) || isPending('create-main:' + p.id); create.title = !saved ? '설정 저장이 확인된 뒤 새 메인을 만들 수 있습니다.' : str(p.create_main?.reason);
    const cancel = button('취소', () => { closeAgentPicker(true); input.focus({preventScroll: true}); }, 'cc-button cc-ghost'); cancel.dataset.ccAgentCancel = ''; actions.append(save, create, cancel); panel.append(actions);
    if (!enabled(p.create_main) && p.create_main?.reason) panel.append(node('p', 'cc-picker-note cc-muted', p.create_main.reason));
    panel.addEventListener('keydown', event => { if (event.key === 'Enter' && ['INPUT', 'SELECT'].includes(event.target.tagName)) { event.preventDefault(); event.stopPropagation(); } });
    form.append(panel); panel.scrollTop = scroll;
    if (focusKey) { const replacement = [...panel.querySelectorAll('[data-cc-focus]')].find(control => control.dataset.ccFocus === focusKey); replacement?.focus({preventScroll: true}); if (selection && textSelection(replacement)) replacement.setSelectionRange(...selection); }
  }
  function navigate(next, selected = null) {
    if (!rememberDraft()) return; agentPicker = null; screen = next; taskId = selected; detailTab = 'work'; closeDrawers(); render(); content.scrollTop = 0;
  }
  function chooseProject(id) {
    if (!rememberDraft()) return; agentPicker = null; workspaceId = id; screen = 'main'; taskId = null; monitorFilter = 'all'; closeDrawers(); render(); content.scrollTop = 0;
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
    agentPicker = null; screen = 'task'; taskId = task.target?.task_id; detailTab = tab === 'result' ? 'work' : tab; closeDrawers(); render(); content.scrollTop = 0;
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
    const heading = node('div', 'cc-side-label'), add = button('＋', () => {}, 'cc-button cc-ghost'); add.disabled = true; add.setAttribute('aria-label', '프로젝트 추가'); add.title = str(snapshot.add_project?.reason) || '이 Mac의 프로젝트 관리에서 프로젝트를 등록해 주세요.'; heading.append(node('span', '', label('workspaces', '워크스페이스')), add);
    left.replaceChildren(heading, button('닫기', closeDrawers, 'cc-button cc-ghost cc-drawer-close'));
    left.querySelector('.cc-drawer-close').dataset.ccDrawerFocus = 'close';
    for (const p of list(snapshot.projects).slice(0, 64)) {
      const group = node('div', 'cc-project-group');
      const row = button('', () => chooseProject(p.id), 'cc-project-row' + (p.id === workspaceId ? ' cc-active' : '')); row.dataset.ccProject = p.id; row.dataset.ccDrawerFocus = 'project:' + p.id;
      row.append(node('span', 'cc-project-glyph', str(p.name).slice(0, 1).toUpperCase()), node('strong', 'cc-grow', p.name), node('span', 'cc-muted', list(p.tasks).length));
      row.setAttribute('aria-current', p.id === workspaceId ? 'true' : 'false'); group.append(row);
      for (const task of list(p.tasks).slice(0, 64)) {
        const entry = button('', () => { if (!rememberDraft()) return; workspaceId = p.id; openTaskDetail(task); }, 'cc-session' + (p.id === workspaceId && screen === 'task' && task.target?.task_id === taskId ? ' cc-selected' : ''));
        entry.dataset.ccSidebarTask = task.target?.task_id || ''; entry.dataset.ccDrawerFocus = 'task:' + routeKey(task.target);
        const meta = node('small'); meta.append(stateTag(task.state, label), document.createTextNode(' · ' + [task.provider, task.device].filter(Boolean).join(' · ')));
        entry.append(node('strong', '', task.title), meta); group.append(entry);
      }
      const sameSession = (a, b) => !!a?.session_id && a.workspace_id === b?.workspace_id && a.session_id === b?.session_id;
      const independent = list(p.sessions).filter(session => session.target?.workspace_id === p.id && session.target?.session_id && !sameSession(session.target, p.main_target) && !list(p.tasks).some(task => sameSession(session.target, task.target)));
      for (const session of independent.slice(0, 64)) {
        const entry = button('', () => {
          if (!rememberDraft()) return;
          if (!session.terminal_session_id || typeof watchSession !== 'function') { say('이 세션의 터미널 연결을 제공하지 않습니다.'); return; }
          workspaceId = p.id; screen = 'main'; taskId = null; agentPicker = null; render(); setOpen(false); watchSession(session.terminal_session_id);
        }, 'cc-session'); entry.dataset.ccSidebarSession = session.target.session_id; entry.dataset.ccDrawerFocus = 'session:' + routeKey(session.target);
        const meta = node('small'); meta.append(stateTag(session.state, label), document.createTextNode(' · ' + [session.provider, session.device].filter(Boolean).join(' · '))); entry.append(node('strong', '', session.title), meta); group.append(entry);
      }
      if (!list(p.tasks).length && !independent.length) group.append(node('p', 'cc-project-empty', '배정된 작업이 없습니다.')); left.append(group);
    }
    const foot = node('div', 'cc-side-foot'); foot.append(button(label('settings', '프로젝트 설정'), () => navigate('settings'), 'cc-button cc-ghost'), button('메인에게 작업 맡기기', () => navigate('main'), 'cc-button cc-ghost'), node('p', '', label('project_independent', '프로젝트·작업·입력은 각각 독립적입니다.'))); foot.querySelector('button').dataset.ccDrawerFocus = 'settings'; foot.querySelector('button').dataset.ccSettings = ''; left.append(foot);
  }
  function taskRow(task) {
    const row = button('', () => navigate('task', task.target?.task_id), 'cc-task-row'); row.dataset.ccTask = task.target?.task_id || '';
    const title = node('div', 'cc-task-title'); title.append(node('strong', '', task.title), node('small', '', task.scope));
    const owner = node('span', 'cc-owner'); owner.append(node('span', '', [task.provider, task.role].filter(Boolean).join(' · ')), node('small', 'cc-muted', task.device));
    row.append(title, owner, stateTag(task.state, label), node('span', 'cc-task-arrow cc-muted', '›'));
    return row;
  }
  function renderRight(p) {
    const previousScroll = right.querySelector('.cc-monitor-scroll')?.scrollTop || 0;
    const tasks = list(p?.tasks), running = tasks.filter(t => t.state === 'running').length;
    const heading = node('header', 'cc-monitor-title'); heading.append(node('h2', '', label('workers', '작업자')), node('small', 'cc-muted', '실행 ' + running + (p?.config ? ' / 최대 ' + p.config.parallel_limit : '')), button(label('close', '닫기'), closeDrawers, 'cc-button cc-ghost cc-drawer-close')); right.replaceChildren(heading); heading.querySelector('button').dataset.ccDrawerFocus = 'close';
    const scroll = node('div', 'cc-monitor-scroll'), filters = node('div', 'cc-monitor-filter'); filters.setAttribute('aria-label', '작업자 필터');
    for (const [key, caption] of [['all', '전체'], ['attention', '확인 필요'], ['running', '실행 중']]) {
      const control = button(caption, () => { monitorFilter = key; render(); }, 'cc-button');
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
    const intro = node('div', 'cc-intro'); intro.append(node('h1', '', caption)); if (description) intro.append(node('p', 'cc-muted', description)); content.append(intro); return intro;
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
    ['목표', '계획·배정', '독립 실행 ' + tasks.filter(t => t.state === 'running').length, '결과 취합', '검증'].forEach((caption, index) => { if (index) flow.append(node('i')); flow.append(node('span', index === 2 ? 'cc-current' : '', caption)); });
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
    const intro = title(task.title, [task.provider, task.role, task.device].filter(Boolean).join(' · ')), heading = node('div', 'cc-detail-heading'); intro.querySelector('p')?.classList.add('cc-detail-meta'); heading.append(intro.querySelector('h1'), stateTag(task.state, label)); intro.prepend(heading);
    const tabs = node('nav', 'cc-tabs'); tabs.setAttribute('aria-label', '작업 상세');
    for (const [key, caption] of [['work', '작업과 결과'], ['terminal', '원문 터미널'], ['git', 'Git 변경'], ['history', '기록']]) {
      const tab = key === 'terminal' ? terminalButton(task) : button(caption, () => { detailTab = key; render(); });
      tab.textContent = caption; tab.className = 'cc-tab' + (detailTab === key ? ' cc-active' : ''); tab.setAttribute('aria-current', detailTab === key ? 'page' : 'false'); tabs.append(tab);
    }
    content.append(tabs);
    if (detailTab === 'terminal' && isRemoteTask(task)) { renderRemoteTerminal(task); return; }
    if (detailTab === 'history') { renderHistory(list(p.history).filter(h => h.target?.task_id === task.target?.task_id)); return; }
    if (detailTab === 'git') { content.append(node('p', 'cc-empty', str(task.git?.reason) || '이 원격 화면에서는 Git 변경 작업을 제공하지 않습니다.')); return; }
    content.append(node('h2', '', '진행 내용'), node('p', 'cc-activity', task.activity), facts([[label('scope', '배정 범위'), task.scope], [label('directory', '작업 폴더'), task.directory], [label('criteria', '완료 조건'), task.acceptance_criteria], ['최근 관측', task.observed_at]]));
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
    dispatch.title = str(task.dispatch?.reason); updateDispatch(); actions.append(select, dispatch, terminalButton(task)); if (choices.length || enabled(task.dispatch) || isPending('dispatch:' + selectionKey)) content.append(actions);
    const result = node('section', 'cc-result-section'); result.append(node('h2', '', '제출된 결과'));
    result.append(node('p', 'cc-result-text', task.result || label('no_result', '아직 제출된 결과가 없습니다.')), facts([['검증 근거', task.verification || '검증 미실행'], ['사용자 확인', task.state === 'completed' ? '확인 완료' : '미확인']]));
    if (task.result_revision) { const actions = node('div', 'cc-actions'); actions.append(actionButton(label('confirm_result', '결과 확인'), task.confirm_result, {type: 'confirm_result', target: task.target, revision: task.result_revision}, 'result:' + routeKey(task.target))); result.append(actions); }
    content.append(result); renderResultForms(task);
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
    const intro = title('프로젝트 설정', '메인과 작업 폴더를 정하고, 필요할 때 실행 규칙을 조정하세요.'), heading = node('div', 'cc-detail-heading'), add = button('프로젝트 추가', () => {}); add.disabled = true; add.title = str(snapshot.add_project?.reason) || '이 Mac의 프로젝트 관리에서 프로젝트를 등록해 주세요.'; heading.append(intro.querySelector('h1'), add); intro.prepend(heading);
    let draft = settings.get(p.id); if (!draft) { if (settings.size >= 64) { content.append(node('p', 'cc-empty', '설정 초안 한도를 넘었습니다.')); return; } draft = {config: copy(p.config), version: 0}; settings.set(p.id, draft); }
    else if (draft.version === 0) draft.config = copy(p.config);
    const fields = node('div', 'cc-form'); let fieldParent = fields;
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
      control.addEventListener(choices || type === 'checkbox' ? 'change' : 'input', () => {
        draft.config[key] = type === 'checkbox' ? control.checked : type === 'paths' ? control.value.split('\n').map(v => v.trim()).filter(Boolean) : key === 'parallel_limit' ? Number(control.value) : key === 'main_session_id' ? control.value || null : control.value;
        if (key === 'main_session_id' && control.value) {
          const selected = list(p.eligible_sessions ?? p.sessions).find(s => s.target?.workspace_id === p.id && s.target?.session_id === control.value);
          if (selected) for (const key of ['provider', 'model', 'effort']) draft.config[key] = str(selected[key]);
        }
        if (['provider', 'model', 'effort'].includes(key)) { draft.config.main_session_id = null; if (key === 'provider') draft.config.model = ''; if (key !== 'effort') draft.config.effort = ''; }
        draft.version++; if (['main_session_id', 'provider', 'model', 'effort'].includes(key)) render();
      });
      row.append(control); if (key === 'directory') row.append(node('small', 'cc-muted', '다른 컴퓨터의 폴더는 기기 연결 시 확인합니다.')); fieldParent.append(row);
    }
    field(label('name', '프로젝트 이름'), 'name'); field('이 Mac의 프로젝트 폴더', 'directory');
    const {sessions, agents} = agentChoices(p), providers = [...new Set(agents.map(a => a.provider))];
    const inline = node('div', 'cc-inline-fields'); fields.append(inline); fieldParent = inline;
    field(label('provider', '메인 에이전트'), 'provider', [['', '에이전트 선택'], ...providers.map(v => [v, v])]);
    const models = [...new Set(agents.filter(a => a.provider === draft.config.provider).map(a => a.model))];
    field(label('model', '모델'), 'model', [['', models.includes('') ? '기본 모델' : '모델 선택'], ...models.filter(Boolean).map(v => [v, v])]);
    const rules = node('details', 'cc-rules'), summary = node('summary', '', '실행 규칙 '); summary.append(node('span', 'cc-rule-summary cc-muted', '· 작업자·병렬 실행·자동화')); rules.dataset.ccDetails = 'settings-rules'; rules.append(summary, node('p', 'cc-rule-note cc-muted', '이 프로젝트의 다음 배정부터 적용합니다.')); fields.append(rules); fieldParent = rules;
    field(label('main_binding', '메인 세션'), 'main_session_id', [['', label('unbound', '연결하지 않음')], ...sessions.map(s => [s.target?.session_id || '', s.title])]);
    const efforts = [...new Set(agents.filter(a => a.provider === draft.config.provider && a.model === draft.config.model).map(a => a.effort))];
    field(label('effort', '추론 강도'), 'effort', [['', efforts.includes('') ? '기본 설정 유지' : '추론 강도 선택'], ...efforts.filter(Boolean).map(v => [v, v])]);
    field('허용 범위 (줄마다 상대 폴더)', 'allowed_scope_roots', null, 'paths');
    field(label('parallel_limit', '동시 작업 수'), 'parallel_limit', null, 'number'); field(label('automation', '자동화'), 'automation', list(p.available_automation).map(v => [v, v])); field(label('worktrees', '작업별 worktree 사용'), 'use_worktrees', null, 'checkbox');
    const workerProviders = node('fieldset', 'cc-providers'); workerProviders.append(node('legend', '', label('worker_providers', '작업자 제공자')));
    for (const provider of list(p.available_providers)) {
      const row = node('label'), control = node('input'); control.type = 'checkbox'; control.checked = list(draft.config.worker_providers).includes(provider); control.disabled = !enabled(p.save_settings) || !connected;
      control.addEventListener('change', () => { const chosen = new Set(draft.config.worker_providers); control.checked ? chosen.add(provider) : chosen.delete(provider); draft.config.worker_providers = [...chosen]; draft.version++; }); row.append(control, node('span', '', provider)); workerProviders.append(row);
    }
    rules.append(workerProviders); fields.append(node('p', 'cc-form-notice', '변경한 설정은 진행 중인 작업의 담당·모델·폴더를 바꾸지 않습니다.'));
    const save = button(label('save', '저장'), () => {
      if (!enabled(p.save_settings)) return;
      if (!Number.isInteger(draft.config.parallel_limit) || draft.config.parallel_limit < 1 || draft.config.parallel_limit > 8 || !draft.config.name.trim() || !draft.config.directory.trim()) { say('이름·폴더와 동시 작업 수 1–8을 확인해 주세요.'); return; }
      emit({type: 'save_project', workspace_id: p.id, config: copy(draft.config)}, 'save:' + p.id, {workspace: p.id, settingsVersion: draft.version});
    }, 'cc-button cc-primary'); save.disabled = !enabled(p.save_settings) || !connected || isPending('save:' + p.id); save.title = str(p.save_settings?.reason);
    const cancel = button('취소', () => { settings.delete(p.id); navigate('main'); }, 'cc-button cc-ghost'); cancel.dataset.ccSettingsCancel = '';
    const actions = node('div', 'cc-actions'); actions.append(save, cancel); fields.append(actions); content.append(fields);
  }
  function renderContent(p) {
    const scroll = content.scrollTop, active = content.contains(document.activeElement) ? document.activeElement : null;
    const focusKey = active?.dataset.ccFocus, selection = textSelection(active);
    const identity = JSON.stringify([workspaceId, screen, taskId, detailTab]);
    const expanded = contentIdentity === identity ? [...content.querySelectorAll('details')].map(detail => detail.open) : []; contentIdentity = identity;
    content.replaceChildren(); content.classList.toggle('cc-settings-screen', screen === 'settings');
    if (screen === 'devices') {
      title('연결된 기기', '열람·입력 권한과 작업 실행 권한은 각각 표시됩니다.');
      if (!list(snapshot.devices).length) content.append(node('p', 'cc-empty', '등록된 기기가 없습니다.'));
      for (const device of list(snapshot.devices).slice(0, 64)) {
        const row = node('div', 'cc-device-row'), body = node('div'), glyph = node('span', 'cc-device-icon'), capability = node('div', 'cc-device-capability cc-muted'); glyph.append(icon('device')); body.append(node('h2', '', device.name), node('small', 'cc-muted', device.description)); capability.append(node('p', '', str(device.capabilities)), node('small', '', device.can_execute ? '작업 실행 가능' : label('viewer_only', '작업 실행 없음'))); row.append(glyph, body, capability, node('span', 'cc-device-state ' + (device.connected ? 'cc-green' : 'cc-amber'), device.connection)); content.append(row);
      }
    } else if (!p) title('프로젝트를 선택해 주세요', '호스트에서 등록한 프로젝트가 이곳에 표시됩니다.');
    else if (screen === 'task') renderTask(p, selectedTask());
    else if (screen === 'settings') renderSettings(p);
    else if (screen === 'history') { title('프로젝트 기록'); renderHistory(list(p.history)); }
    else if (screen === 'tasks') {
      title('프로젝트 작업', p.name + ' · ' + list(p.tasks).length + '개 독립 작업');
      if (!list(p.tasks).length) content.append(node('p', 'cc-empty', '메인에게 목표를 보내 작업을 배정하세요.'));
      else { const table = node('div', 'cc-task-table'), labels = node('div', 'cc-task-labels'); for (const caption of ['나눈 작업', '담당 · 실행 기기', '상태', '']) labels.append(node('span', '', caption)); table.append(labels); for (const task of list(p.tasks).slice(0, 64)) table.append(taskRow(task)); content.append(table); }
    }
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
      const caption = key === 'tasks' ? fallback + ' ' + list(project()?.tasks).length : fallback;
      const tab = button(label('nav_' + key, caption), () => key === 'terminal' ? openCurrentTerminal() : navigate(key), 'cc-nav-button' + (active ? ' cc-active' : '')); tab.dataset.ccNav = key; tab.setAttribute('aria-current', active ? 'page' : 'false'); nav.append(tab);
    }
    const p = project(); renderLeft(); renderRight(p); projectbar.replaceChildren();
    const heading = node('div', 'cc-grow'), scope = node('span', 'cc-path'); scope.append(node('span', 'cc-accent', '●'), document.createTextNode(' '), node('code', '', p?.directory || ''), document.createTextNode(' · 프로젝트 범위')); heading.append(node('h2', '', p?.name || '프로젝트 없음'), scope);
    const select = node('select', 'cc-project-select'); select.setAttribute('aria-label', '작업할 프로젝트'); for (const available of list(snapshot.projects).slice(0, 64)) { const option = node('option', '', available.name); option.value = available.id; select.append(option); } select.value = p?.id || ''; select.addEventListener('change', () => chooseProject(select.value));
    const settings = button('프로젝트 설정 ↗', () => navigate('settings'), 'cc-button cc-ghost'); settings.dataset.ccSettings = ''; settings.setAttribute('aria-label', '프로젝트 설정');
    const toggle = button('◫', () => { monitorCollapsed = !monitorCollapsed; render(); }, 'cc-button cc-ghost cc-monitor-toggle'); toggle.dataset.ccMonitorToggle = ''; toggle.setAttribute('aria-label', monitorCollapsed ? '작업자 패널 펼치기' : '작업자 패널 접기'); toggle.setAttribute('aria-expanded', String(!monitorCollapsed));
    const controls = node('div', 'cc-project-controls'); controls.append(node('label', 'cc-muted', '작업할 프로젝트'), select, settings, toggle); projectbar.append(heading, controls);
    context.replaceChildren();
    const openLeft = button('', () => openDrawer('left', openLeft), 'cc-button cc-ghost cc-open-left'); openLeft.append(icon('menu')); openLeft.dataset.ccOpenLeft = ''; openLeft.setAttribute('aria-label', '프로젝트 목록 열기');
    const caption = node('div', 'cc-grow'), captionTitle = node('h2', '', screen === 'task' ? [selectedTask()?.provider, selectedTask()?.role].filter(Boolean).join(' · ') || '작업' : screen === 'main' ? '메인 에이전트' : label(screen, {tasks: '프로젝트 작업', devices: '기기', history: '프로젝트 기록', settings: '프로젝트 설정'}[screen])); caption.append(captionTitle);
    if (screen === 'main') captionTitle.append(node('small', 'cc-context-model', p?.main_target?.session_id ? [p?.main_provider, [p?.main_model, p?.main_effort].filter(Boolean).join(' ')].filter(Boolean).join(' · ') : '메인 에이전트 선택'));
    const openRight = button('작업자 ' + list(p?.tasks).length, () => openDrawer('right', openRight), 'cc-button cc-ghost cc-open-right'); openRight.dataset.ccOpenRight = ''; openRight.setAttribute('aria-label', '작업자 패널 열기');
    const history = button('전체 기록', () => navigate('history'), 'cc-button cc-ghost cc-all-history'); history.dataset.ccContextHistory = ''; history.hidden = screen === 'history';
    context.append(openLeft, caption, history); if (screen !== 'main') context.append(button('메인으로', () => navigate('main'), 'cc-button cc-ghost')); context.append(openRight);
    deviceCount.textContent = '기기 ' + list(snapshot.devices).filter(device => device.connected).length + '대 연결';
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
    if (event.key === 'Escape') { if (agentPicker) { event.preventDefault(); event.stopPropagation(); closeAgentPicker(); targetCaption.focus({preventScroll: true}); } else closeDrawers(); return; }
    if (event.key !== 'Tab' || !drawer) return;
    const panel = drawer === 'left' ? left : right; if (!panel.hasAttribute('aria-modal')) return;
    const controls = [...panel.querySelectorAll('button:not(:disabled),select:not(:disabled),input:not(:disabled),[tabindex="0"]')].filter(control => control.getClientRects().length), first = controls[0], last = controls.at(-1);
    if (event.shiftKey && (document.activeElement === first || !panel.contains(document.activeElement))) { event.preventDefault(); last?.focus(); }
    else if (!event.shiftKey && (document.activeElement === last || !panel.contains(document.activeElement))) { event.preventDefault(); first?.focus(); }
  }
  element.addEventListener('keydown', onKey);
  const onPickerOutside = event => { if (agentPicker && !form.querySelector('[data-cc-agent-picker]')?.contains(event.target) && !targetCaption.contains(event.target)) closeAgentPicker(); };
  element.addEventListener('pointerdown', onPickerOutside);
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
      if (destroyed) return; destroyed = true; syncRemoteWatch(); observer?.disconnect(); element.removeEventListener('keydown', onKey); element.removeEventListener('pointerdown', onPickerOutside); content.removeEventListener('scroll', rememberRemotePosition, true); document.removeEventListener('visibilitychange', syncRemoteWatch); pending.clear(); drafts.clear(); settings.clear(); plans.clear(); resultDrafts.clear(); workerSelections.clear(); remotePositions.clear(); agentDrafts.clear(); element.remove(); notifyVisibility(false);
    },
  };
}
