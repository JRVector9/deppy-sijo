// Shared operator results view. This view only emits App-owned cloud intents.
export function createCloudResultsView({ host, send, navigate = () => {}, beginNavigation = () => {}, onVisibility = () => {}, prefix = 'cloud' }) {
  const node = (tag, className, text) => {
    const element = document.createElement(tag);
    if (className) element.className = className;
    if (text !== undefined) element.textContent = String(text);
    return element;
  };
  const toggle = node('button', 'cloud-results-toggle', '클라우드 작업 · 결과');
  toggle.id = prefix + '-results-toggle'; toggle.type = 'button'; toggle.hidden = true;
  const panel = node('section', 'cloud-results'); panel.id = prefix + '-results'; panel.hidden = true;
  toggle.setAttribute('aria-controls', panel.id); toggle.setAttribute('aria-expanded', 'false');
  const heading = node('h2', '', '클라우드 작업 · 결과'); heading.id = prefix + '-results-title';
  panel.setAttribute('aria-labelledby', heading.id);
  const help = node('p', 'cloud-help', '클라우드 보고와 Mac 터미널 상태를 따로 표시합니다. 취소 요청은 에이전트의 확인을 기다립니다.');
  const toolbar = node('div', 'cloud-toolbar');
  const search = node('input'); search.type = 'search'; search.placeholder = '연결 · 작업 · 결과 검색'; search.setAttribute('aria-label', search.placeholder);
  const filter = node('select'); filter.setAttribute('aria-label', '클라우드 작업 상태 필터');
  for (const [value, label] of [['all','전체'],['active','진행 · 미확인'],['results','결과 있음'],['unknown','상태 미확인']]) {
    const option = node('option', '', label); option.value = value; filter.append(option);
  }
  toolbar.append(search, filter);
  const status = node('p', 'cloud-feedback'); status.setAttribute('role', 'status'); status.setAttribute('aria-live', 'polite');
  const retained = node('p', 'cloud-retained');
  const list = node('div', 'cloud-results-list');
  panel.append(heading, help, toolbar, status, retained, list); host.append(toggle, panel);
  let snapshot = null, canControl = false, canApprove = false, sequence = 0;
  const pending = new Map();
  const rows = value => Array.isArray(value) ? value.slice(0, 100) : [];
  const normalize = value => String(value || '').normalize('NFC').toLocaleLowerCase();
  const stateLabels = { queued:'접수됨', running:'진행 중', waiting:'대기 중', completed:'완료 보고', failed:'실패 보고', cancelled:'취소 보고', unknown:'상태 미확인' };
  const localLabels = { running:'실행 중', waiting:'입력 대기', needs_approval:'승인 필요', idle:'유휴', error:'오류', done:'완료 관측' };
  const connectionLabels = { active:'인증됨', waiting:'인증 대기', revoked:'취소됨', expired:'인증 만료' };
  const scopeLabel = value => ['input','read+input'].includes(value) ? '입력 허용' : '읽기 전용';
  const matches = row => !search.value || normalize([row.title,row.body,row.result,row.message,row.name,row.connection_name,row.id,row.connection_id,row.workspace_id,row.session_id].join(' ')).includes(normalize(search.value));
  const terminal = row => ['completed','failed','cancelled'].includes(row.state);
  const keyOf = intent => JSON.stringify(intent);
  function action(label, intent, allowed = canControl) {
    const button = node('button', '', label); button.type = 'button'; button.dataset.cloudAction = intent.action;
    button.disabled = !allowed || Array.from(pending.values()).some(entry => entry.key === keyOf(intent));
    button.addEventListener('click', () => {
      if (button.disabled || !snapshot || pending.size >= 16) return;
      const request = sequence = sequence >= 0xffffffff ? 1 : sequence + 1;
      if (send({ type:'cloud_control', request, intent }) === false) { status.textContent = '연결을 확인하고 다시 요청하세요.'; return; }
      if (intent.action === 'navigate') { cancelNavigation(); beginNavigation(); }
      button.disabled = true;
      const timer = setTimeout(() => {
        if (!pending.delete(request)) return;
        status.textContent = '요청 접수를 확인하지 못했습니다. 최신 상태를 확인하세요.'; render();
      }, 15000);
      pending.set(request, { key:keyOf(intent), timer, intent });
      status.textContent = '요청을 보냈습니다. 최신 상태로 결과를 확인합니다.';
    });
    return button;
  }
  function artifacts(card, references) {
    for (const reference of rows(references).slice(0, 16)) {
      let url;
      try { url = new URL(reference.uri); } catch { continue; }
      if (!['https:','http:'].includes(url.protocol) || url.username || url.password || !url.hostname) continue;
      const link = node('a', 'cloud-artifact', reference.label || '결과물'); link.href = url.href; link.target = '_blank'; link.rel = 'noopener noreferrer';
      card.append(link);
    }
  }
  function source(card, row) {
    card.append(node('p', 'cloud-source', '클라우드 · ' + (row.connection_name || row.connection_id || '연결 정보 없음') + ' · ' + row.workspace_id + ' / ' + row.session_id));
    card.append(node('p', 'cloud-generation', '원래 실행 · ' + (row.generation || '정보 없음')));
  }
  function originalActions(card, row, cancellable) {
    const actions = node('div', 'cloud-actions');
    actions.append(action('원래 세션으로 이동', { action:'navigate', workspace_id:row.workspace_id, session_id:row.session_id }));
    if (cancellable) {
      actions.append(action('입력 제어 회수', { action:'take_control', session_id:row.session_id, generation:row.generation }));
      actions.append(action('취소 요청', { action:'cancel_run', connection_id:row.connection_id, run_id:row.id }, canControl && !terminal(row) && !row.cancel_requested_at));
    }
    card.append(actions);
  }
  function render() {
    list.replaceChildren();
    if (!snapshot) return;
    toggle.hidden = false;
    retained.textContent = snapshot.truncated ? '최근 항목만 표시합니다. 각 목록 최대 100개입니다.' : '최근 작업과 저장 답변 · 각 목록 최대 100개';
    let count = 0;
    if (filter.value === 'all') {
      for (const connection of rows(snapshot.connections).filter(matches)) {
        const card = node('article', 'cloud-card cloud-connection');
        card.append(node('h3', '', connection.name || connection.id));
        card.append(node('p', 'cloud-source', '클라우드 연결 · ' + (connectionLabels[connection.state] || connection.state || '상태 미확인') + ' · ' + scopeLabel(connection.input_scope)));
        card.append(action('연결 취소', { action:'revoke_connection', connection_id:connection.id }, canControl && connection.state !== 'revoked'));
        list.append(card); count++;
      }
      for (const approval of (canApprove ? rows(snapshot.approvals) : []).filter(matches)) {
        const card = node('article', 'cloud-card'); card.append(node('h3', '', '연결 승인 · ' + approval.connection_name));
        card.append(node('p', 'cloud-source', '요청 범위 · ' + scopeLabel(approval.scope)));
        const actions = node('div', 'cloud-actions');
        actions.append(action('연결 승인', { action:'approve_connection', approval_id:approval.id, allow:true }, canApprove), action('거절', { action:'approve_connection', approval_id:approval.id, allow:false }, canApprove));
        card.append(actions); list.append(card); count++;
      }
    }
    for (const run of rows(snapshot.runs).filter(matches)) {
      if (filter.value === 'active' && terminal(run)) continue;
      if (filter.value === 'results' && !run.result && !rows(run.artifacts).length) continue;
      if (filter.value === 'unknown' && run.state !== 'unknown' && !run.stale) continue;
      const card = node('article', 'cloud-card'); card.dataset.cloudRun = run.id;
      card.append(node('h3', '', run.title || run.id)); source(card, run);
      const reported = stateLabels[run.state] || '상태 미확인';
      card.append(node('p', 'cloud-reported', '클라우드 보고 · ' + reported + (run.progress === undefined || run.progress === null ? '' : ' · ' + run.progress + '%')));
      card.append(node('p', 'cloud-local', '로컬 터미널 · ' + (localLabels[run.local_status] || run.local_status || '관측 정보 없음')));
      if (run.stale) card.append(node('p', 'cloud-warning', '보고가 오래됨 · 결과 확인 필요'));
      if (run.cancel_acknowledged_at) card.append(node('p', 'cloud-cancel', '취소 확인됨 · 에이전트 확인 보고'));
      else if (run.cancel_requested_at) card.append(node('p', 'cloud-cancel', '취소 요청됨 · 에이전트 확인 대기'));
      if (run.message) card.append(node('p', 'cloud-body', run.message));
      if (run.result) card.append(node('p', 'cloud-body', run.result));
      artifacts(card, run.artifacts); originalActions(card, run, true); list.append(card); count++;
    }
    if (['all','results'].includes(filter.value)) for (const answer of rows(snapshot.answers).filter(matches)) {
      const card = node('article', 'cloud-card'); card.dataset.cloudAnswer = answer.id;
      card.append(node('h3', '', answer.title || '저장 답변')); source(card, answer);
      card.append(node('p', 'cloud-source', '에이전트 자체 답변 · ' + (answer.run_id || '작업 ID 없음')));
      card.append(node('p', 'cloud-body', answer.body)); artifacts(card, answer.artifacts); originalActions(card, answer, false); list.append(card); count++;
    }
    if (!count) list.append(node('p', 'cloud-empty', search.value ? '검색 결과가 없습니다.' : '저장된 클라우드 작업이 없습니다.'));
  }
  toggle.addEventListener('click', () => {
    panel.hidden = !panel.hidden; toggle.setAttribute('aria-expanded', String(!panel.hidden)); onVisibility(!panel.hidden);
  });
  search.addEventListener('input', render); filter.addEventListener('change', render);
  function cancelNavigation() {
    for (const [request, entry] of pending) {
      if (entry.intent.action !== 'navigate') continue;
      clearTimeout(entry.timer); pending.delete(request);
    }
  }
  function resetConnection() {
    canControl = canApprove = false;
    for (const entry of pending.values()) clearTimeout(entry.timer);
    pending.clear(); status.textContent = '연결 끊김 · 마지막 수신 상태'; render();
  }
  function accept(message) {
    if (message.type === 'cloud_snapshot') {
      snapshot = message.snapshot || {}; canControl = message.can_control === true; canApprove = message.can_approve === true;
      status.textContent = canControl ? '연결됨 · 요청은 Mac에서 권한을 확인합니다.' : canApprove ? '승인 허용 · 터미널 읽기 전용' : '읽기 전용 · 연결된 기기의 권한으로 표시합니다.'; render(); return;
    }
    if (message.type !== 'cloud_control_result') return;
    const entry = pending.get(message.request); if (!entry) return;
    clearTimeout(entry.timer); pending.delete(message.request);
    status.textContent = message.status === 'queued' ? '요청 접수됨 · 최신 상태와 에이전트 확인을 기다립니다.' : '요청이 거절되었습니다. 최신 권한과 대상을 확인하세요.';
    if (message.status === 'queued' && entry.intent.action === 'navigate') navigate(entry.intent);
    render();
  }
  return { accept, resetConnection, cancelNavigation, panel, toggle };
}
