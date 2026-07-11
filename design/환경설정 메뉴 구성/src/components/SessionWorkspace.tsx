import { useState, type ReactNode } from 'react'
import { egui, type EguiTokens, type Theme } from './egui'
import ComponentGuide from './ComponentGuide'
import Sidebar from './Sidebar'
import EnvProfilePanel from './EnvProfilePanel'

type Page = 'components' | 'settingsFull' | 'settingsFullV2' | 'workspaceV1' | 'workspaceV2' | 'workspaceV3' | 'workspaceV4' | 'scenarios' | 'deppyScenarios' | 'rename' | 'close' | 'menu'
type SessionStatus = 'running' | 'idle'
type FileState = 'modified' | 'new' | 'clean'

interface Session {
  id: string
  name: string
  model: string
  status: SessionStatus
  accent: string
  selected?: boolean
}

interface TreeNode {
  name: string
  type: 'folder' | 'file'
  state?: FileState
  selected?: boolean
  children?: TreeNode[]
}

const PAGE_LABELS: { id: Page; label: string }[] = [
  { id: 'components', label: '컴포넌트 요소' },
  { id: 'settingsFull', label: '환경설정 전체' },
  { id: 'settingsFullV2', label: '환경설정 전체 v2' },
  { id: 'workspaceV1', label: '전체 화면 v1' },
  { id: 'workspaceV2', label: '전체 화면 v2' },
  { id: 'workspaceV3', label: '전체 화면 v3' },
  { id: 'workspaceV4', label: '개발 시작 v4' },
  { id: 'scenarios', label: '사용 시나리오' },
  { id: 'deppyScenarios', label: 'Deppy 연계' },
  { id: 'rename', label: '이름 변경' },
  { id: 'close', label: 'pane 닫기' },
  { id: 'menu', label: '컨텍스트 메뉴' },
]

const SESSIONS: Session[] = [
  { id: 'users', name: 'Users', model: 'Codex · gpt-5.5 · medium', status: 'running', accent: '#4da6c8' },
  { id: 'design', name: 'Design', model: 'Codex · gpt-5.5 · medium', status: 'idle', accent: '#66c587', selected: true },
  { id: 'auction', name: '경매작가', model: 'Codex · gpt-5.5 · medium', status: 'idle', accent: '#66c587' },
  { id: 'tennis', name: 'tennisssss', model: 'Claude · claude-opus-4-8', status: 'running', accent: '#4da6c8' },
]

const FOLDERS = [
  'Aijanggi',
  'AgentServer',
  'CVdata',
  'Colon',
  'Crawler',
  'Deppy_AiBox',
  'Insta-donwloader',
  'Insta-donwloader 복사본',
  'LLM-release',
  'Learning_Math',
  'MetaPage',
  'Mynie',
  'Naver_land',
  'Onetwoday',
  'VisionAI',
  'VisionAI2',
]

const POPUP_RULES = {
  width: 520,
  titleHeight: 96,
  bodyMinHeight: 182,
  bodyPadding: 28,
  titleFontSize: 36,
  messageFontSize: 26,
  buttonHeight: 58,
  buttonMinWidth: 112,
  buttonGap: 16,
}

const AGENT_EVENTS = [
  { time: '15:27:02', kind: 'system', label: 'system', text: 'session resumed · gpt-5.5 high · /Users' },
  { time: '15:27:04', kind: 'user', label: 'user', text: '전체화면을 에이전트 전용 터미널로 다시 구성' },
  { time: '15:27:06', kind: 'plan', label: 'plan', text: 'read components → map egui tokens → compose workspace shell → verify build' },
  { time: '15:27:12', kind: 'tool', label: 'tool', text: 'rg --files src · 9 files scanned' },
  { time: '15:27:18', kind: 'tool', label: 'tool', text: 'sed -n ComponentGuide.tsx · token rules loaded' },
  { time: '15:27:24', kind: 'tool', label: 'tree', text: 'selected file · src/components/SessionWorkspace.tsx · modified' },
  { time: '15:27:29', kind: 'assistant', label: 'agent', text: '폴더 트리에서 선택한 파일 기준으로 로그, 도구 상태, 컨텍스트를 연결합니다.' },
  { time: '15:27:34', kind: 'warn', label: 'warn', text: 'ahto MCP startup incomplete · local design work continues' },
  { time: '15:27:48', kind: 'success', label: 'done', text: 'workspace draft ready · hot reload pending' },
]

const TOOL_ROWS = [
  ['fs', 'read/write', 'ready'],
  ['git', 'status/diff', 'dirty'],
  ['vite', 'dev server', '8443'],
  ['build', 'pnpm build', 'pass'],
]

const QUEUE_ROWS = [
  ['active', 'Design', 'gpt-5.5 high'],
  ['idle', '경매작가', 'gpt-5.5 medium'],
  ['running', 'Users', 'gpt-5.5 medium'],
]

const FILE_TREE: TreeNode[] = [
  {
    name: '환경설정 메뉴 구성',
    type: 'folder',
    children: [
      {
        name: 'src',
        type: 'folder',
        children: [
          { name: 'App.tsx', type: 'file', state: 'modified' },
          {
            name: 'components',
            type: 'folder',
            children: [
              { name: 'SessionWorkspace.tsx', type: 'file', state: 'modified', selected: true },
              { name: 'ComponentGuide.tsx', type: 'file', state: 'clean' },
              { name: 'egui.ts', type: 'file', state: 'clean' },
              { name: 'ProjectList.tsx', type: 'file', state: 'clean' },
            ],
          },
          { name: 'index.css', type: 'file', state: 'clean' },
        ],
      },
      {
        name: '.figma',
        type: 'folder',
        children: [
          { name: 'make/site.json', type: 'file', state: 'new' },
        ],
      },
      { name: 'package.json', type: 'file', state: 'clean' },
      { name: 'vite.config.ts', type: 'file', state: 'clean' },
    ],
  },
]

const TASK_BOARD = [
  {
    id: 'T-104',
    status: 'active',
    title: '전체 화면 v3 구성',
    detail: 'task queue 중심의 에이전트 터미널 재설계',
    owner: 'Design',
    progress: 68,
  },
  {
    id: 'T-103',
    status: 'review',
    title: '팝업 컴포넌트 규칙화',
    detail: 'titleHeight, bodyMinHeight, centered actions',
    owner: 'Design',
    progress: 100,
  },
  {
    id: 'T-102',
    status: 'queued',
    title: 'Codex 출력 구조화',
    detail: 'stdout 파싱 대신 event stream 기반 렌더링',
    owner: 'Users',
    progress: 12,
  },
  {
    id: 'T-101',
    status: 'blocked',
    title: '실제 queue runtime 연결',
    detail: '세션 이벤트 API 또는 로컬 broker 필요',
    owner: 'System',
    progress: 0,
  },
]

const TASK_TIMELINE = [
  ['18:56:02', 'input', '사용자 요청 수신', 'task queue에 올릴 수 있는 내용과 새 화면 구성'],
  ['18:56:07', 'plan', '작업 단위 분해', 'answer · version preserve · v3 screen · build verify'],
  ['18:56:16', 'read', '현재 컴포넌트 확인', 'SessionWorkspace.tsx, egui tokens'],
  ['18:56:31', 'edit', '화면 추가', 'workspaceV3 route and task board shell'],
  ['18:56:44', 'verify', '검증 대기', 'pnpm build'],
]

const IMPACT_FILES = [
  ['modified', 'src/components/SessionWorkspace.tsx', '+ v3 workspace, task board'],
  ['modified', 'src/App.tsx', 'entry remains SessionWorkspace'],
  ['new', '.figma/make/site.json', 'local vite compatibility'],
]

const QUEUE_CAPABILITIES = [
  'user request',
  'plan step',
  'tool call',
  'file edit',
  'test/build',
  'browser check',
  'approval wait',
  'blocked item',
]

const START_SKILLS = [
  ['browser', '로컬 화면 열기, 클릭, 스크린샷 확인', 'ready'],
  ['figma', '컴포넌트/화면을 Figma로 옮기기', 'available'],
  ['openai-docs', 'OpenAI API/모델 문서 확인', 'available'],
  ['pdf/docs', '문서/PDF 생성과 렌더 검증', 'available'],
  ['obsidian', '결정사항과 남은 작업 기록', 'available'],
  ['prod/dev deploy', '개발/운영 배포 흐름 실행', 'guarded'],
]

const QUICK_ACTIONS = [
  ['화면 확인', '브라우저에서 localhost:8443 열기', 'browser'],
  ['빌드 검증', 'pnpm build 실행 후 오류 요약', 'build'],
  ['파일 찾기', 'rg로 컴포넌트/토큰 위치 검색', 'search'],
  ['디자인 정리', 'egui 토큰 기준으로 UI 재구성', 'design'],
  ['남은 작업 기록', '검토사항을 노트로 저장', 'note'],
  ['배포 준비', '환경/상태/로그 확인 후 배포', 'deploy'],
]

const START_CHECKLIST = [
  ['server', 'Vite dev server', 'running · 8443'],
  ['entry', '현재 화면', 'SessionWorkspace.tsx'],
  ['theme', '스타일 시스템', 'egui tokens'],
  ['build', '마지막 검증', 'passed'],
  ['dirty', '작업트리', 'design files modified'],
]

const RECENT_CONTEXT = [
  ['요청', '에이전트 전용 터미널 UI 설계'],
  ['방향', '실제 개발 시작에 필요한 정보 중심'],
  ['주의', '실시간 데이터는 runtime event 연결 필요'],
]

const USAGE_SCENARIOS = [
  {
    id: 'screen-edit',
    title: '시나리오 1 · 화면 수정 시작',
    goal: '사용자가 앱 화면을 보고 디자인 수정을 요청한 뒤, 빌드와 브라우저 확인까지 한 번에 진행한다.',
    prompt: '전체 화면을 더 현실적인 에이전트 터미널처럼 다시 구성해줘. 기존 버전은 남겨줘.',
    skill: 'browser + build + design',
    steps: [
      ['1', '상태 확인', 'Start Checklist에서 dev server, entry file, 마지막 build 상태를 확인한다.'],
      ['2', '컴포넌트 읽기', 'Project Tree에서 SessionWorkspace.tsx와 egui.ts를 선택해 스타일 규칙을 확인한다.'],
      ['3', '작업 큐 등록', 'TASK QUEUE에 “전체 화면 v3/v4 구성” 작업이 active로 올라간다.'],
      ['4', '구현', 'Quick Action “디자인 정리”를 실행해 egui 토큰 기준으로 화면을 수정한다.'],
      ['5', '검증', 'Quick Action “빌드 검증”과 “화면 확인”으로 build/browser 상태를 확인한다.'],
    ],
    result: '새 버전 탭이 추가되고, 이전 버전은 유지되며, 검증 결과가 context panel에 남는다.',
  },
  {
    id: 'build-fix',
    title: '시나리오 2 · 빌드 실패 해결',
    goal: '코드 수정 중 HMR 또는 build 오류가 발생했을 때 원인 파일을 찾고 수정한 뒤 재검증한다.',
    prompt: '화면이 안 떠. 오류 로그 보고 원인 찾아서 고쳐줘.',
    skill: 'search + build + browser',
    steps: [
      ['1', '오류 감지', 'Start Checklist의 build 상태가 failed로 바뀌고 TASK QUEUE에 “빌드 실패 해결”이 추가된다.'],
      ['2', '로그 정리', '중앙 timeline에 에러 위치, 컴포넌트 이름, stack trace 요약이 표시된다.'],
      ['3', '파일 추적', 'Impact Files에서 오류 파일을 선택하고 Project Tree에서 해당 위치를 연다.'],
      ['4', '수정', 'Quick Action “파일 찾기”로 심볼을 찾고 코드 수정 작업을 실행한다.'],
      ['5', '재검증', 'pnpm build와 browser refresh가 통과하면 TASK 상태가 done으로 바뀐다.'],
    ],
    result: '오류 원인, 수정 파일, 검증 명령이 한 화면에 남아 다음 작업자가 이어받을 수 있다.',
  },
]

const DEPPY_SETUP = [
  ['login', '웹/모바일 로그인', 'DeviceLogin 또는 세션으로 계정 연결'],
  ['brain', 'LLM 연결', '구독 브레인 또는 BYOK provider/model 지정'],
  ['tools', '생활 도구', 'Gmail, Calendar, Drive, Docs, Sheets, MCP'],
  ['notify', '모바일 알림', 'in-app, email, Slack, Discord, Telegram, Kakao'],
  ['guard', '승인 규칙', '메일 발송/삭제/결제/외부 쓰기는 pause 후 승인'],
  ['memory', '워크스페이스 메모리', '반복 선호, 사람, 프로젝트 맥락 저장'],
]

const DEPPY_SCENARIOS = [
  {
    id: 'morning-brief',
    title: '시나리오 1 · 아침 운영 브리핑',
    promise: '컴퓨터가 꺼져 있어도 07:00에 클라우드 런이 실행되고 모바일로 결과가 온다.',
    trigger: 'cron · weekday 07:00',
    mobile: 'Kakao + in-app',
    steps: [
      ['07:00', 'ScheduledTaskFire', 'worker가 SubscriptionRun을 만들고 agent runtime을 깨운다.'],
      ['07:01', 'read connectors', 'Gmail unread, Calendar today, Drive 최근 문서를 읽는다.'],
      ['07:03', 'compose report', '오늘 일정, 답장 필요 메일, 준비할 문서를 하나의 리포트로 묶는다.'],
      ['07:04', 'notify mobile', '모바일 알림에 요약과 “자세히 보기/승인” 액션을 붙인다.'],
      ['07:06', 'memory update', '반복되는 일정/사람/프로젝트 선호를 workspace memory에 반영한다.'],
    ],
    result: '출근 전 모바일에서 오늘 처리해야 할 일, 급한 답장, 회의 준비물을 확인한다.',
  },
  {
    id: 'away-command',
    title: '시나리오 2 · 외출 중 업무 지시',
    promise: '모바일에서 “A에게 회의 자료 보내줘”라고 지시하면 서버에서 실행되고 위험 작업은 승인 대기한다.',
    trigger: 'mobile command · chat/deeplink',
    mobile: 'in-app approval',
    steps: [
      ['14:12', 'prompt queue', '모바일 입력이 AgentRun queued 상태로 올라간다.'],
      ['14:13', 'context load', 'Drive/Docs에서 회의 자료 후보를 찾고 Calendar 참석자를 확인한다.'],
      ['14:14', 'draft action', 'Gmail 초안을 만들고 외부 발송 전 AgentRunPause를 생성한다.'],
      ['14:15', 'await input', '사용자는 모바일에서 본문을 보고 approve/edit/reject를 선택한다.'],
      ['14:16', 'resume run', '승인되면 발송하고 Notification과 RunEvent에 결과를 남긴다.'],
    ],
    result: '노트북 없이도 자료 검색, 초안 작성, 승인 후 발송까지 이어진다.',
  },
]

const DEPPY_TERMINAL = [
  ['local', '$ deppy login --device', 'userCode: J7R-K29 · approve on mobile'],
  ['cloud', 'device approved', 'workspace: personal · user linked'],
  ['setup', 'connect gmail calendar drive kakao', 'oauth connected · 4 tools ready'],
  ['policy', 'guardrail external_write=require_approval', 'send/delete/pay actions pause'],
  ['schedule', 'schedule daily-brief "0 7 * * 1-5"', 'next fire: 2026-07-09 07:00 KST'],
  ['run', 'run sub_8421 queued → running', 'computer_off=true · cloud_runtime=true'],
  ['pause', 'awaiting_input email.send', 'mobile approval requested'],
  ['done', 'report delivered', 'notification: kakao · run succeeded'],
]

const DEPPY_BOUNDARIES = [
  ['가능', '예약 실행', 'ScheduledTask + BullMQ worker + SubscriptionRun'],
  ['가능', '모바일 확인', 'NotificationChannel과 agent run detail 화면'],
  ['가능', '승인 후 실행', 'AgentRunPause, awaiting_input, resume flow'],
  ['조건부', '앱 푸시', '현재 채널은 준비되어 있고 PWA/native push UX가 필요'],
  ['조건부', '로컬 Codex 작업', '노트북이 꺼지면 불가. Deppy cloud run으로 분리해야 함'],
]

export default function SessionWorkspace() {
  const [theme, setTheme] = useState<Theme>('dark')
  const [page, setPage] = useState<Page>('settingsFull')
  const t = egui(theme)
  const isComponentsPage = page === 'components'

  return (
    <div style={{
      width: '100%',
      minHeight: '100vh',
      backgroundColor: t.bg,
      fontFamily: '"JetBrains Mono", "Fira Mono", "Consolas", monospace',
      color: t.text,
    }}>
      <TopNavigation theme={theme} onThemeChange={setTheme} page={page} onPageChange={setPage} t={t} />
      <main style={isComponentsPage ? {
        minHeight: 'calc(100vh - 42px)',
      } : {
        minHeight: 'calc(100vh - 42px)',
        display: 'flex',
        alignItems: 'center',
        justifyContent: 'center',
        padding: 24,
        boxSizing: 'border-box',
      }}>
        {page === 'components' && <ComponentGuide />}
        {page === 'settingsFull' && <SettingsFullPage theme={theme} onThemeChange={setTheme} t={t} />}
        {page === 'settingsFullV2' && <SettingsFullPageV2 theme={theme} onThemeChange={setTheme} t={t} />}
        {page === 'workspaceV1' && <WorkspacePageV1 t={t} />}
        {page === 'workspaceV2' && <WorkspacePage t={t} />}
        {page === 'workspaceV3' && <WorkspacePageV3 t={t} />}
        {page === 'workspaceV4' && <WorkspacePageV4 t={t} />}
        {page === 'scenarios' && <ScenariosPage t={t} />}
        {page === 'deppyScenarios' && <DeppyScenariosPage t={t} />}
        {page === 'rename' && <FocusedPage title="Image #2 · 이름 변경" t={t}><RenamePage t={t} /></FocusedPage>}
        {page === 'close' && <FocusedPage title="Image #3 · pane 닫기" t={t}><ClosePanePage t={t} /></FocusedPage>}
        {page === 'menu' && <FocusedPage title="Image #4 · 컨텍스트 메뉴" t={t}><ContextMenuPage t={t} /></FocusedPage>}
      </main>
    </div>
  )
}

function TopNavigation({
  theme,
  onThemeChange,
  page,
  onPageChange,
  t,
}: {
  theme: Theme
  onThemeChange: (theme: Theme) => void
  page: Page
  onPageChange: (page: Page) => void
  t: EguiTokens
}) {
  return (
    <div style={{
      height: 42,
      display: 'flex',
      alignItems: 'center',
      gap: 12,
      padding: '0 16px',
      borderBottom: `1px solid ${t.border}`,
      backgroundColor: t.panel,
      boxSizing: 'border-box',
    }}>
      <SegmentedControl items={PAGE_LABELS} active={page} onChange={onPageChange} t={t} />
      <span style={{ marginLeft: 'auto', fontSize: 10, color: t.muted, letterSpacing: '0.04em' }}>
        egui tokens · shared component states
      </span>
      <SegmentedControl
        items={[
          { id: 'light', label: '라이트' },
          { id: 'dark', label: '다크' },
        ]}
        active={theme}
        onChange={onThemeChange}
        t={t}
      />
    </div>
  )
}

function SegmentedControl<T extends string>({
  items,
  active,
  onChange,
  t,
}: {
  items: { id: T; label: string }[]
  active: T
  onChange: (value: T) => void
  t: EguiTokens
}) {
  return (
    <div style={{ display: 'flex', gap: 1, border: `1px solid ${t.border}` }}>
      {items.map(item => {
        const selected = item.id === active
        return (
          <button
            key={item.id}
            onClick={() => onChange(item.id)}
            style={{
              border: 'none',
              padding: '4px 12px',
              backgroundColor: selected ? t.accent : t.input,
              color: selected ? t.accentText : t.muted,
              fontFamily: 'inherit',
              fontSize: 11,
              cursor: 'pointer',
            }}
          >
            {item.label}
          </button>
        )
      })}
    </div>
  )
}

function WorkspacePage({ t }: { t: EguiTokens }) {
  return (
    <WindowFrame t={t} width={1340} height={800}>
      <div style={{
        display: 'grid',
        gridTemplateColumns: '278px minmax(0, 1fr) 286px',
        height: '100%',
        minWidth: 0,
      }}>
        <AgentRail t={t} />
        <AgentTerminal t={t} />
        <AgentInspector t={t} />
      </div>
    </WindowFrame>
  )
}

function WorkspacePageV1({ t }: { t: EguiTokens }) {
  return (
    <WindowFrame t={t} width={1280} height={760}>
      <div style={{ display: 'flex', height: '100%', minWidth: 0 }}>
        <SessionSidebar t={t} />
        <TerminalArea t={t} />
      </div>
    </WindowFrame>
  )
}

function SettingsFullPage({
  theme,
  onThemeChange,
  t,
}: {
  theme: Theme
  onThemeChange: (theme: Theme) => void
  t: EguiTokens
}) {
  const [activeNav, setActiveNav] = useState('일반')
  const [search, setSearch] = useState('')

  return (
    <WindowFrame t={t} width={1280} height={760}>
      <div style={{
        height: '100%',
        minWidth: 0,
        display: 'flex',
        overflow: 'hidden',
        backgroundColor: t.surface,
      }}>
        <Sidebar
          theme={theme}
          activeNav={activeNav}
          onSelect={setActiveNav}
          search={search}
          onSearch={setSearch}
        />
        <div style={{ flex: 1, minWidth: 0, display: 'flex', overflow: 'hidden' }}>
          <div style={{ flex: 1, minWidth: 0, minHeight: 0, overflow: 'hidden', backgroundColor: t.surface }}>
            {activeNav === '일반' && <GeneralSettingsPage theme={theme} onThemeChange={onThemeChange} t={t} />}
            {activeNav === '언어' && <LanguageSettingsPage t={t} />}
            {activeNav === '터미널' && <TerminalSettingsPage t={t} />}
            {activeNav === '성능' && <PerformanceSettingsPage t={t} />}
            {activeNav === '원격 서버 (TLS)' && <RemoteTlsSettingsPage t={t} />}
            {activeNav === '자격증명' && <CredentialsSettingsPage t={t} />}
            {activeNav === '연결 (커넥터)' && <ConnectorSettingsPage t={t} />}
            {activeNav === '에이전트' && <AgentSettingsPage t={t} />}
            {activeNav === '워크스페이스' && <WorkspaceSettingsPage t={t} />}
            {activeNav === '활동' && <ActivitySettingsPage t={t} />}
            {activeNav === '알림' && <NotificationSettingsPage t={t} />}
            {activeNav === '환경 및 API' && <EnvProfilePanel theme={theme} />}
            {activeNav === '환경 설정' && <EnvProfilePanel theme={theme} />}
            {activeNav !== '일반' && activeNav !== '언어' && activeNav !== '터미널' && activeNav !== '성능' && activeNav !== '원격 서버 (TLS)' && activeNav !== '자격증명' && activeNav !== '연결 (커넥터)' && activeNav !== '에이전트' && activeNav !== '워크스페이스' && activeNav !== '활동' && activeNav !== '알림' && activeNav !== '환경 및 API' && activeNav !== '환경 설정' && (
              <SettingsPlaceholder activeNav={activeNav} t={t} />
            )}
          </div>
        </div>
      </div>
    </WindowFrame>
  )
}

function SettingsFullPageV2({
  theme,
  onThemeChange,
  t,
}: {
  theme: Theme
  onThemeChange: (theme: Theme) => void
  t: EguiTokens
}) {
  const [activeNav, setActiveNav] = useState('환경 및 API')
  const [search, setSearch] = useState('')

  return (
    <WindowFrame t={t} width={1280} height={760}>
      <div style={{
        height: '100%',
        minWidth: 0,
        display: 'flex',
        overflow: 'hidden',
        backgroundColor: t.surface,
      }}>
        <Sidebar
          theme={theme}
          activeNav={activeNav}
          onSelect={setActiveNav}
          search={search}
          onSearch={setSearch}
        />
        <div style={{ flex: 1, minWidth: 0, display: 'flex', overflow: 'hidden' }}>
          <div style={{ flex: 1, minWidth: 0, minHeight: 0, overflow: 'hidden', backgroundColor: t.surface }}>
            {activeNav === '일반' && <GeneralSettingsPage theme={theme} onThemeChange={onThemeChange} t={t} />}
            {activeNav === '언어' && <LanguageSettingsPage t={t} />}
            {activeNav === '터미널' && <TerminalSettingsPage t={t} />}
            {activeNav === '성능' && <PerformanceSettingsPage t={t} />}
            {activeNav === '원격 서버 (TLS)' && <RemoteTlsSettingsPage t={t} />}
            {activeNav === '자격증명' && <CredentialsSettingsPage t={t} />}
            {activeNav === '연결 (커넥터)' && <ConnectorSettingsPage t={t} />}
            {activeNav === '환경 및 API' && <EnvProfilePanel theme={theme} />}
            {activeNav === '환경 설정' && <EnvProfilePanel theme={theme} />}
            {activeNav === '에이전트' && <AgentSettingsPage t={t} />}
            {activeNav === '워크스페이스' && <WorkspaceSettingsPage t={t} />}
            {activeNav === '활동' && <ActivitySettingsPage t={t} />}
            {activeNav === '알림' && <NotificationSettingsPage t={t} />}
          </div>
        </div>
      </div>
    </WindowFrame>
  )
}

function GeneralSettingsPage({
  theme,
  onThemeChange,
  t,
}: {
  theme: Theme
  onThemeChange: (theme: Theme) => void
  t: EguiTokens
}) {
  return (
    <SettingsDetailShell title="모양" t={t}>
      <SettingsRow
        title="테마"
        description="창 크롬만 전환, 터미널 pane은 항상 다크"
        t={t}
        control={
          <SettingsSegmented
            t={t}
            items={[
              { label: '▣ 시스템', value: 'system' },
              { label: '☀ 라이트', value: 'light' },
              { label: '● 다크', value: 'dark' },
            ]}
            active={theme}
            onChange={value => {
              if (value === 'light' || value === 'dark') onThemeChange(value)
            }}
          />
        }
      />
      <SettingsRow
        title="UI 폰트"
        description="사이드바·설정 등 UI 텍스트 폰트 (한글 지원 시스템 폰트)"
        t={t}
        control={<SettingsSelect label="AppleGothic" t={t} />}
      />
      <SettingsRow
        title="폴더 트리 사이드바"
        description="OFF면 Panel 미생성 — 리소스 0"
        t={t}
        control={<SettingsToggle checked t={t} />}
      />
      <SettingsRow
        title="세션 자동 이어가기"
        description="재시작 시 이전 claude/codex 세션을 resume 명령으로 자동 실행"
        t={t}
        control={<SettingsToggle checked t={t} />}
      />
      <SettingsRow
        title="에이전트 상태 hook"
        description="claude/codex 설정에 hook을 설치해 승인/입력 대기를 정확히 감지"
        t={t}
        control={<SettingsToggle checked t={t} />}
      />
    </SettingsDetailShell>
  )
}

function LanguageSettingsPage({ t }: { t: EguiTokens }) {
  const [open, setOpen] = useState(true)
  const [locale, setLocale] = useState('한국어')

  return (
    <SettingsDetailShell title="언어" t={t}>
      <SettingsRow
        title="로케일"
        description="한국어 · 영어 · 일본어 · 중국어 간체/번체 (언어팩 번들)"
        t={t}
        control={
          <SettingsDropdown
            value={locale}
            open={open}
            options={['영어', '일본어', '중국어 간체', '중국어 번체', '한국어']}
            onToggle={() => setOpen(value => !value)}
            onSelect={value => {
              setLocale(value)
              setOpen(false)
            }}
            t={t}
          />
        }
      />
      <SettingsRow
        title="에이전트 응답 언어"
        description="새 세션에서 assistant 기본 응답 언어를 UI 로케일과 동기화"
        t={t}
        control={<SettingsToggle checked t={t} />}
      />
      <SettingsRow
        title="폴더명 표시"
        description="파일 트리에서 한글 조합 문자와 긴 경로를 안정적으로 렌더링"
        t={t}
        control={<SettingsSelect label="시스템 기준" t={t} />}
      />
      <SettingsRow
        title="날짜/시간 형식"
        description="로그, task queue, scheduled run 시간 표기"
        t={t}
        control={<SettingsSelect label="YYYY-MM-DD HH:mm" t={t} />}
      />
    </SettingsDetailShell>
  )
}

function TerminalSettingsPage({ t }: { t: EguiTokens }) {
  return (
    <SettingsDetailShell title="Terminal" t={t}>
      <SettingsRow
        title="폰트 크기"
        description="터미널 출력 영역과 입력 composer의 기본 글자 크기"
        t={t}
        control={<SettingsStepper value="12.5" t={t} />}
      />
      <SettingsRow
        title="스크롤백 줄 수"
        description="hidden 세션은 자동으로 1,000줄로 축소"
        t={t}
        control={<SettingsStepper value="20,000" t={t} />}
      />
      <SettingsRow
        title="줄 간격"
        description="출력 로그, tool event, warning line의 수직 밀도"
        t={t}
        control={<SettingsSelect label="1.35" t={t} />}
      />
    </SettingsDetailShell>
  )
}

function PerformanceSettingsPage({ t }: { t: EguiTokens }) {
  return (
    <SettingsDetailShell title="Performance" t={t}>
      <SettingsRow
        title="출력 배치 간격(ms)"
        description="0·1은 16ms로 정규화 (idle repaint 억제)"
        t={t}
        control={<SettingsStepper value="25 ms" t={t} />}
      />
      <SettingsRow
        title="백그라운드 pane 절전"
        description="비활성 pane의 diff, tree, terminal repaint를 지연"
        t={t}
        control={<SettingsToggle checked t={t} />}
      />
      <SettingsRow
        title="파일 트리 갱신"
        description="대형 워크스페이스에서는 변경 이벤트를 묶어서 처리"
        t={t}
        control={<SettingsSelect label="250 ms debounce" t={t} />}
      />
    </SettingsDetailShell>
  )
}

function RemoteTlsSettingsPage({ t }: { t: EguiTokens }) {
  return (
    <SettingsDetailShell title="Remote (TLS)" t={t}>
      <SettingsRow
        title="TLS 원격 서버 사용"
        description="토글 off/on 후 적용"
        t={t}
        control={<SettingsToggle checked t={t} />}
      />
      <SettingsRow
        title="포트 (0 = 임의)"
        description="로컬 TLS 서버가 바인딩할 포트"
        t={t}
        control={<SettingsTextInput value="0" compact t={t} />}
      />
      <SettingsRow
        title="주소"
        description="클라이언트에서 접속할 로컬 엔드포인트"
        t={t}
        control={<SettingsValue value="127.0.0.1:52297" t={t} />}
      />
      <SettingsInfoRow
        title="지문 (SHA-256):"
        value="4d:17:c3:7a:f9:b0:48:34:67:3d:1e:1e:49:a9:77:1c:e3:e8:d2:01:6e:86:64:20:9c:cf:2a:ab:11:a1:30:04"
        t={t}
      />
      <SettingsRow
        title="토큰"
        description="숨김 상태입니다. 표시를 선택해 확인한 뒤 드래그 선택 후 복사하세요."
        t={t}
        control={<SettingsCheckbox label="표시" t={t} />}
      />
      <SettingsInfoBlock
        title="known_hosts"
        lines={[
          '/Users/jr/Library/Application Support/app.vector9.deppy-sijo/known_hosts',
          '신뢰 기록 없음',
          '클라이언트에서 attach_tls_tofu를 사용하고 첫 접속 시 위 지문과 대조하세요.',
        ]}
        t={t}
      />
    </SettingsDetailShell>
  )
}

function ConnectorSettingsPage({ t }: { t: EguiTokens }) {
  return (
    <SettingsDetailShell title="Local MCP" t={t}>
      <SettingsInlineStatus
        title="111 111"
        status="미확인"
        action="연결 테스트"
        t={t}
      />
      <SettingsSubsectionTitle title="MCP 서버 추가 (stdio)" t={t} />
      <SettingsFormRow label="이름" t={t}>
        <SettingsTextInput value="" align="left" t={t} />
      </SettingsFormRow>
      <SettingsFormRow label="command" t={t}>
        <SettingsTextInput value="" align="left" wide t={t} />
      </SettingsFormRow>
      <SettingsFormRow label="args" description="한 줄에 하나. secret은 args가 아니라 자격증명/환경으로 넣으세요." t={t}>
        <SettingsTextArea value={'-y\nserver-filesystem'} t={t} />
      </SettingsFormRow>
      <div style={{ padding: '8px 0 24px', borderBottom: `1px solid ${t.border}` }}>
        <SettingsButton label="추가" t={t} />
      </div>

      <SettingsSubsectionTitle title="OAuth 커넥터" t={t} />
      <SettingsFormRow label="이름" t={t}>
        <SettingsTextInput value="" align="left" t={t} />
      </SettingsFormRow>
      <SettingsFormRow label="authorize URL" t={t}>
        <SettingsTextInput value="" align="left" wide t={t} />
      </SettingsFormRow>
      <SettingsFormRow label="token URL" t={t}>
        <SettingsTextInput value="" align="left" wide t={t} />
      </SettingsFormRow>
      <SettingsFormRow label="client id" t={t}>
        <SettingsTextInput value="" align="left" t={t} />
      </SettingsFormRow>
      <SettingsFormRow label="scopes" description="공백 구분" t={t}>
        <SettingsTextInput value="" align="left" wide t={t} />
      </SettingsFormRow>
      <div style={{ padding: '8px 0 0' }}>
        <SettingsButton label="브라우저로 연결" t={t} />
      </div>
    </SettingsDetailShell>
  )
}

function CredentialsSettingsPage({ t }: { t: EguiTokens }) {
  return (
    <SettingsDetailShell title="자격증명" t={t}>
      <SettingsRow
        title="기본 키 저장소"
        description="API 키와 토큰을 저장할 로컬 vault 위치"
        t={t}
        control={<SettingsSelect label="system keychain" t={t} />}
      />
      <SettingsRow
        title="OpenAI API 키"
        description="모델 호출과 테스트 실행에 사용할 기본 키"
        t={t}
        control={<SettingsTextInput value="sk-••••••••••••••••" align="left" t={t} />}
      />
      <SettingsRow
        title="GitHub 토큰"
        description="repo status, issue, PR 확인에 사용할 personal token"
        t={t}
        control={<SettingsTextInput value="ghp_••••••••••••" align="left" t={t} />}
      />
      <SettingsRow
        title="외부 쓰기 보호"
        description="메일 발송, 배포, 삭제 작업 전 승인 대기 상태로 전환"
        t={t}
        control={<SettingsToggle checked t={t} />}
      />
    </SettingsDetailShell>
  )
}

function AgentSettingsPage({ t }: { t: EguiTokens }) {
  return (
    <SettingsDetailShell title="에이전트" t={t}>
      <SettingsRow
        title="기본 모델"
        description="새 에이전트 세션을 시작할 때 사용할 모델"
        t={t}
        control={<SettingsSelect label="gpt-5.5 high" t={t} />}
      />
      <SettingsRow
        title="승인 정책"
        description="외부 쓰기, 파일 삭제, 배포 실행 전 확인"
        t={t}
        control={<SettingsSelect label="risky actions only" t={t} />}
      />
      <SettingsRow
        title="동시 실행 수"
        description="한 워크스페이스에서 동시에 실행 가능한 agent run 수"
        t={t}
        control={<SettingsStepper value="3" t={t} />}
      />
      <SettingsRow
        title="실패 시 알림"
        description="run failed, approval pending, budget warning 이벤트를 알림으로 전달"
        t={t}
        control={<SettingsToggle checked t={t} />}
      />
    </SettingsDetailShell>
  )
}

function WorkspaceSettingsPage({ t }: { t: EguiTokens }) {
  return (
    <SettingsDetailShell title="워크스페이스" t={t}>
      <SettingsRow
        title="기본 작업 경로"
        description="새 세션이 시작되는 루트 디렉터리"
        t={t}
        control={<SettingsTextInput value="~/Desktop/Projects" align="left" wide t={t} />}
      />
      <SettingsRow
        title="파일 변경 감지"
        description="대형 repo에서 파일 트리와 변경 목록을 갱신하는 방식"
        t={t}
        control={<SettingsSelect label="watch + debounce" t={t} />}
      />
      <SettingsFormRow label="제외 패턴" description="한 줄에 하나씩 입력" t={t}>
        <SettingsTextArea value={'node_modules\ndist\n.env\n.DS_Store'} t={t} />
      </SettingsFormRow>
      <SettingsRow
        title="작업 상태 자동 저장"
        description="pane, active file, selected task 상태를 다음 실행 때 복구"
        t={t}
        control={<SettingsToggle checked t={t} />}
      />
    </SettingsDetailShell>
  )
}

function ActivitySettingsPage({ t }: { t: EguiTokens }) {
  return (
    <SettingsDetailShell title="활동" t={t}>
      <SettingsRow
        title="이벤트 보존 기간"
        description="agent run, tool call, approval event 기록 보존"
        t={t}
        control={<SettingsSelect label="30 days" t={t} />}
      />
      <SettingsRow
        title="로그 상세도"
        description="터미널에 표시할 도구 출력과 시스템 이벤트 범위"
        t={t}
        control={<SettingsSelect label="normal" t={t} />}
      />
      <SettingsRow
        title="실시간 tail"
        description="활성 세션에서 stdout, stderr, event stream을 자동 스크롤"
        t={t}
        control={<SettingsToggle checked t={t} />}
      />
      <SettingsInlineStatus title="최근 내보내기" status="없음" action="내보내기" t={t} />
    </SettingsDetailShell>
  )
}

function NotificationSettingsPage({ t }: { t: EguiTokens }) {
  return (
    <SettingsDetailShell title="알림" t={t}>
      <SettingsRow
        title="앱 내 알림"
        description="완료, 실패, 승인 대기 이벤트를 우측 패널과 모바일 web에 표시"
        t={t}
        control={<SettingsToggle checked t={t} />}
      />
      <SettingsRow
        title="이메일"
        description="scheduled task 결과와 장시간 실행 실패를 메일로 전달"
        t={t}
        control={<SettingsSelect label="daily summary" t={t} />}
      />
      <SettingsRow
        title="Kakao / Telegram"
        description="모바일에서 승인/거절/자세히 보기로 이어지는 알림 채널"
        t={t}
        control={<SettingsSelect label="not connected" t={t} />}
      />
      <SettingsRow
        title="예산 경고"
        description="토큰/실행 예산이 임계치에 도달하면 알림"
        t={t}
        control={<SettingsToggle checked t={t} />}
      />
    </SettingsDetailShell>
  )
}

function SettingsDetailShell({ title, t, children }: { title: string; t: EguiTokens; children: ReactNode }) {
  return (
    <div style={{
      height: '100%',
      overflowY: 'auto',
      backgroundColor: t.input,
      padding: '20px 26px 40px',
      boxSizing: 'border-box',
    }}>
      <div style={{ color: t.text, fontSize: 15, fontWeight: 600, marginBottom: 16 }}>{title}</div>
      <div style={{ borderTop: `1px solid ${t.border}` }}>
        {children}
      </div>
    </div>
  )
}

function SettingsRow({
  title,
  description,
  control,
  t,
}: {
  title: string
  description: string
  control: ReactNode
  t: EguiTokens
}) {
  return (
    <div style={{
      minHeight: 68,
      display: 'grid',
      gridTemplateColumns: 'minmax(0, 1fr) 360px',
      gap: 20,
      alignItems: 'center',
      borderBottom: `1px solid ${t.border}`,
      padding: '10px 0',
      boxSizing: 'border-box',
    }}>
      <div style={{ minWidth: 0 }}>
        <div style={{ color: t.text, fontSize: 14, fontWeight: 500, lineHeight: 1.35 }}>{title}</div>
        <div style={{ marginTop: 5, color: t.textSecondary, fontSize: 13, lineHeight: 1.45, wordBreak: 'keep-all' }}>{description}</div>
      </div>
      <div style={{ display: 'flex', justifyContent: 'flex-end', minWidth: 0 }}>{control}</div>
    </div>
  )
}

function SettingsStepper({ value, t }: { value: string; t: EguiTokens }) {
  return (
    <div style={{
      height: 34,
      display: 'grid',
      gridTemplateColumns: '138px 44px 44px',
      border: `1px solid ${t.inputBorder}`,
      backgroundColor: t.surface,
      fontVariantNumeric: 'tabular-nums',
    }}>
      <span style={{
        display: 'flex',
        alignItems: 'center',
        justifyContent: 'center',
        color: t.text,
        fontSize: 14,
        borderRight: `1px solid ${t.inputBorder}`,
      }}>
        {value}
      </span>
      <button style={{
        border: 'none',
        borderRight: `1px solid ${t.inputBorder}`,
        backgroundColor: 'transparent',
        color: t.muted,
        fontFamily: 'inherit',
        fontSize: 14,
      }}>
        -
      </button>
      <button style={{
        border: 'none',
        backgroundColor: 'transparent',
        color: t.muted,
        fontFamily: 'inherit',
        fontSize: 14,
      }}>
        +
      </button>
    </div>
  )
}

function SettingsSegmented<T extends string>({
  items,
  active,
  onChange,
  t,
}: {
  items: { label: string; value: T }[]
  active: T | string
  onChange: (value: T) => void
  t: EguiTokens
}) {
  return (
    <div style={{
      display: 'flex',
      border: `1px solid ${t.inputBorder}`,
      backgroundColor: t.surface,
      height: 34,
    }}>
      {items.map((item, index) => {
        const selected = item.value === active || (item.value === 'system' && active !== 'light' && active !== 'dark')
        return (
          <button
            key={item.value}
            onClick={() => onChange(item.value)}
            style={{
              minWidth: 104,
              padding: '0 14px',
              border: 'none',
              borderLeft: index === 0 ? 'none' : `1px solid ${t.border}`,
              backgroundColor: selected ? t.navActive : 'transparent',
              color: selected ? t.accent : t.textSecondary,
              fontFamily: 'inherit',
              fontSize: 14,
              cursor: 'pointer',
            }}
          >
            {item.label}
          </button>
        )
      })}
    </div>
  )
}

function SettingsInlineStatus({
  title,
  status,
  action,
  t,
}: {
  title: string
  status: string
  action: string
  t: EguiTokens
}) {
  return (
    <div style={{
      minHeight: 68,
      display: 'grid',
      gridTemplateColumns: 'minmax(0, 1fr) 120px 156px',
      gap: 12,
      alignItems: 'center',
      borderBottom: `1px solid ${t.border}`,
      padding: '10px 0',
      boxSizing: 'border-box',
    }}>
      <div style={{ color: t.text, fontSize: 14, fontWeight: 500 }}>{title}</div>
      <div style={{ color: t.textSecondary, fontSize: 14 }}>{status}</div>
      <SettingsButton label={action} t={t} />
    </div>
  )
}

function SettingsSubsectionTitle({ title, t }: { title: string; t: EguiTokens }) {
  return (
    <div style={{
      padding: '14px 0 8px',
      borderBottom: `1px solid ${t.border}`,
      color: t.text,
      fontSize: 14,
      fontWeight: 500,
    }}>
      {title}
    </div>
  )
}

function SettingsFormRow({
  label,
  description,
  children,
  t,
}: {
  label: string
  description?: string
  children: ReactNode
  t: EguiTokens
}) {
  return (
    <div style={{
      minHeight: description ? 68 : 52,
      display: 'grid',
      gridTemplateColumns: '154px minmax(0, 1fr)',
      gap: 12,
      alignItems: 'center',
      borderBottom: `1px solid ${t.border}`,
      padding: '8px 0',
      boxSizing: 'border-box',
    }}>
      <div style={{ minWidth: 0 }}>
        <div style={{ color: t.text, fontSize: 14, lineHeight: 1.35 }}>{label}</div>
        {description && (
          <div style={{ marginTop: 4, color: t.textSecondary, fontSize: 13, lineHeight: 1.4 }}>{description}</div>
        )}
      </div>
      <div style={{ minWidth: 0 }}>{children}</div>
    </div>
  )
}

function SettingsButton({ label, t }: { label: string; t: EguiTokens }) {
  return (
    <button style={{
      minWidth: 90,
      height: 34,
      padding: '0 14px',
      border: `1px solid ${t.border}`,
      backgroundColor: t.navActive,
      color: t.text,
      fontFamily: 'inherit',
      fontSize: 14,
      cursor: 'pointer',
      borderRadius: 2,
    }}>
      {label}
    </button>
  )
}

function SettingsTextInput({
  value,
  compact,
  wide,
  align = 'right',
  t,
}: {
  value: string
  compact?: boolean
  wide?: boolean
  align?: 'left' | 'right'
  t: EguiTokens
}) {
  return (
    <div style={{
      width: compact ? 80 : wide ? 560 : 230,
      maxWidth: '100%',
      height: 32,
      display: 'flex',
      alignItems: 'center',
      justifyContent: align === 'left' ? 'flex-start' : 'flex-end',
      padding: '0 10px',
      boxSizing: 'border-box',
      border: `1px solid ${t.inputBorder}`,
      backgroundColor: t.surface,
      color: t.text,
      fontSize: 14,
      fontVariantNumeric: 'tabular-nums',
    }}>
      {value}
    </div>
  )
}

function SettingsTextArea({ value, t }: { value: string; t: EguiTokens }) {
  return (
    <div style={{
      width: 560,
      maxWidth: '100%',
      minHeight: 70,
      padding: '8px 10px',
      boxSizing: 'border-box',
      border: `1px solid ${t.inputBorder}`,
      backgroundColor: t.surface,
      color: t.textSecondary,
      fontSize: 14,
      lineHeight: 1.45,
      whiteSpace: 'pre-wrap',
    }}>
      {value}
    </div>
  )
}

function SettingsValue({ value, t }: { value: string; t: EguiTokens }) {
  return (
    <span style={{
      color: t.text,
      fontSize: 14,
      fontVariantNumeric: 'tabular-nums',
      overflow: 'hidden',
      textOverflow: 'ellipsis',
      whiteSpace: 'nowrap',
      maxWidth: 360,
    }}>
      {value}
    </span>
  )
}

function SettingsCheckbox({ label, t }: { label: string; t: EguiTokens }) {
  return (
    <button style={{
      display: 'flex',
      alignItems: 'center',
      gap: 8,
      border: 'none',
      backgroundColor: 'transparent',
      color: t.text,
      fontFamily: 'inherit',
      fontSize: 14,
      cursor: 'pointer',
    }}>
      <span style={{
        width: 16,
        height: 16,
        border: `1px solid ${t.inputBorder}`,
        backgroundColor: t.surface,
        borderRadius: 2,
        boxSizing: 'border-box',
      }} />
      {label}
    </button>
  )
}

function SettingsInfoRow({ title, value, t }: { title: string; value: string; t: EguiTokens }) {
  return (
    <div style={{
      minHeight: 86,
      borderBottom: `1px solid ${t.border}`,
      padding: '14px 0',
      boxSizing: 'border-box',
    }}>
      <div style={{ color: t.text, fontSize: 14, fontWeight: 500 }}>{title}</div>
      <div style={{
        marginTop: 10,
        color: t.text,
        fontSize: 14,
        lineHeight: 1.5,
        wordBreak: 'break-all',
        fontVariantNumeric: 'tabular-nums',
      }}>
        {value}
      </div>
    </div>
  )
}

function SettingsInfoBlock({ title, lines, t }: { title: string; lines: string[]; t: EguiTokens }) {
  return (
    <div style={{ padding: '18px 0 0' }}>
      <div style={{ color: t.text, fontSize: 14, fontWeight: 500, marginBottom: 10 }}>{title}</div>
      <div style={{ color: t.textSecondary, fontSize: 14, lineHeight: 1.65, wordBreak: 'break-all' }}>
        {lines.map(line => <div key={line}>{line}</div>)}
      </div>
    </div>
  )
}

function SettingsSelect({ label, t }: { label: string; t: EguiTokens }) {
  return (
    <button style={{
      width: 230,
      height: 34,
      display: 'flex',
      alignItems: 'center',
      justifyContent: 'space-between',
      padding: '0 12px',
      border: `1px solid ${t.inputBorder}`,
      backgroundColor: t.surface,
      color: t.text,
      fontFamily: 'inherit',
      fontSize: 14,
      cursor: 'pointer',
    }}>
      <span style={{ overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>{label}</span>
      <span style={{ color: t.muted }}>▾</span>
    </button>
  )
}

function SettingsToggle({ checked, t }: { checked: boolean; t: EguiTokens }) {
  return (
    <button style={{
      width: 46,
      height: 26,
      border: `1px solid ${checked ? t.accent : t.border}`,
      backgroundColor: checked ? t.accent : t.surface,
      display: 'flex',
      alignItems: 'center',
      justifyContent: checked ? 'flex-end' : 'flex-start',
      padding: 3,
      boxSizing: 'border-box',
      cursor: 'pointer',
      borderRadius: 2,
    }}>
      <span style={{
        width: 18,
        height: 18,
        backgroundColor: checked ? t.accentText : t.muted,
        display: 'block',
        borderRadius: 2,
      }} />
    </button>
  )
}

function SettingsDropdown({
  value,
  open,
  options,
  onToggle,
  onSelect,
  t,
}: {
  value: string
  open: boolean
  options: string[]
  onToggle: () => void
  onSelect: (value: string) => void
  t: EguiTokens
}) {
  return (
    <div style={{ width: 230, position: 'relative' }}>
      <button
        onClick={onToggle}
        style={{
          width: '100%',
          height: 34,
          display: 'flex',
          alignItems: 'center',
          justifyContent: 'space-between',
          padding: '0 12px',
          border: `1px solid ${t.inputBorder}`,
          backgroundColor: t.surface,
          color: t.text,
          fontFamily: 'inherit',
          fontSize: 14,
          cursor: 'pointer',
        }}
      >
        <span>{value}</span>
        <span style={{ color: t.muted }}>▾</span>
      </button>
      {open && (
        <div style={{
          position: 'absolute',
          zIndex: 4,
          top: 35,
          left: 0,
          right: 0,
          border: `1px solid ${t.inputBorder}`,
          backgroundColor: t.panel,
          padding: 6,
          boxShadow: '0 8px 24px rgba(0,0,0,0.35)',
        }}>
          {options.map(option => {
            const selected = option === value
            return (
              <button
                key={option}
                onClick={() => onSelect(option)}
                style={{
                  width: '100%',
                  height: 31,
                  display: 'flex',
                  alignItems: 'center',
                  padding: '0 8px',
                  border: 'none',
                  backgroundColor: selected ? t.accent : 'transparent',
                  color: selected ? t.accentText : t.textSecondary,
                  fontFamily: 'inherit',
                  fontSize: 14,
                  textAlign: 'left',
                  cursor: 'pointer',
                }}
              >
                {option}
              </button>
            )
          })}
        </div>
      )}
    </div>
  )
}

function SettingsPlaceholder({ activeNav, t }: { activeNav: string; t: EguiTokens }) {
  return (
    <div style={{
      height: '100%',
      display: 'grid',
      gridTemplateRows: '116px 40px minmax(0, 1fr)',
      backgroundColor: t.input,
    }}>
      <div style={{ padding: 18, borderBottom: `1px solid ${t.border}` }}>
        <div style={{ color: t.text, fontSize: 15, fontWeight: 600 }}>{activeNav}</div>
        <div style={{ marginTop: 8, color: t.textSecondary, fontSize: 13, lineHeight: 1.6 }}>
          이 영역은 기존 환경설정 전체화면의 확장 슬롯입니다. 지금은 환경 설정 화면을 실제 컴포넌트로 연결했고,
          나머지 메뉴는 같은 레이아웃 규칙으로 채울 수 있게 자리를 고정했습니다.
        </div>
      </div>
      <SectionHeader label="SETTING SECTIONS" t={t} />
      <div style={{ padding: 14, display: 'grid', gridTemplateColumns: 'repeat(3, minmax(0, 1fr))', gap: 10, alignContent: 'start' }}>
        {[
          ['상태', '저장됨 · 동기화 대기 없음'],
          ['권한', '읽기/쓰기 범위와 승인 정책'],
          ['연동', '커넥터와 워크스페이스 연결'],
          ['감사', '변경 내역과 최근 활동'],
          ['검증', '환경 변수 누락, 키 만료 확인'],
          ['내보내기', '프로필 복사, 공유, 백업'],
        ].map(([label, value]) => (
          <div key={label} style={{ border: `1px solid ${t.border}`, backgroundColor: t.panel, padding: 12, minHeight: 84 }}>
            <div style={{ color: t.accent, fontSize: 14, fontWeight: 500 }}>{label}</div>
            <div style={{ marginTop: 8, color: t.textSecondary, fontSize: 13, lineHeight: 1.5 }}>{value}</div>
          </div>
        ))}
      </div>
    </div>
  )
}

function WorkspacePageV3({ t }: { t: EguiTokens }) {
  return (
    <WindowFrame t={t} width={1380} height={820}>
      <div style={{
        height: '100%',
        minWidth: 0,
        display: 'grid',
        gridTemplateRows: '44px minmax(0, 1fr)',
        backgroundColor: t.input,
      }}>
        <V3CommandBar t={t} />
        <div style={{
          minHeight: 0,
          display: 'grid',
          gridTemplateColumns: '336px minmax(0, 1fr) 318px',
        }}>
          <TaskQueuePanel t={t} />
          <TaskExecutionPanel t={t} />
          <TaskContextPanel t={t} />
        </div>
      </div>
    </WindowFrame>
  )
}

function WorkspacePageV4({ t }: { t: EguiTokens }) {
  return (
    <WindowFrame t={t} width={1380} height={820}>
      <div style={{
        height: '100%',
        minWidth: 0,
        display: 'grid',
        gridTemplateRows: '48px minmax(0, 1fr)',
        backgroundColor: t.input,
      }}>
        <StartHeader t={t} />
        <div style={{
          minHeight: 0,
          display: 'grid',
          gridTemplateColumns: '320px minmax(0, 1fr) 340px',
        }}>
          <StartSidebar t={t} />
          <StartMain t={t} />
          <StartAssistantPanel t={t} />
        </div>
      </div>
    </WindowFrame>
  )
}

function StartHeader({ t }: { t: EguiTokens }) {
  return (
    <div style={{
      display: 'grid',
      gridTemplateColumns: '320px minmax(0, 1fr) 340px',
      borderBottom: `1px solid ${t.border}`,
      backgroundColor: t.panel,
    }}>
      <div style={{ display: 'flex', alignItems: 'center', gap: 8, padding: '0 12px', borderRight: `1px solid ${t.border}` }}>
        <span style={{ color: t.accent }}>◆</span>
        <span style={{ color: t.text, fontSize: 14, fontWeight: 700 }}>Start Console</span>
        <span style={{ marginLeft: 'auto', color: t.muted, fontSize: 10 }}>project-ready</span>
      </div>
      <div style={{ display: 'flex', alignItems: 'center', padding: '0 14px', gap: 12 }}>
        <span style={{ color: t.muted, fontSize: 11 }}>next best action</span>
        <span style={{ color: t.text, fontSize: 14, fontWeight: 700 }}>화면 확인 → 빌드 검증 → 변경 파일 정리</span>
      </div>
      <div style={{ display: 'flex', alignItems: 'center', padding: '0 12px', borderLeft: `1px solid ${t.border}`, gap: 8 }}>
        <span style={{ color: t.muted, fontSize: 11 }}>live sources</span>
        <span style={{ marginLeft: 'auto', color: t.textSecondary, fontSize: 11 }}>mock · can connect</span>
      </div>
    </div>
  )
}

function StartSidebar({ t }: { t: EguiTokens }) {
  return (
    <aside style={{ minHeight: 0, display: 'grid', gridTemplateRows: '40px 170px 40px minmax(0, 1fr)', borderRight: `1px solid ${t.border}`, backgroundColor: t.panel }}>
      <SectionHeader label="START CHECKLIST" t={t} />
      <div style={{ padding: 10, borderBottom: `1px solid ${t.border}` }}>
        {START_CHECKLIST.map(([key, label, value]) => (
          <ChecklistRow key={key} label={label} value={value} t={t} />
        ))}
      </div>
      <SectionHeader label="PROJECT TREE" t={t} action={<span style={{ color: t.muted, fontSize: 10 }}>focus file</span>} />
      <div style={{ minHeight: 0, overflowY: 'auto', padding: '6px 0' }}>
        {FILE_TREE.map(node => <TreeRow key={node.name} node={node} depth={0} t={t} />)}
      </div>
    </aside>
  )
}

function ChecklistRow({ label, value, t }: { label: string; value: string; t: EguiTokens }) {
  return (
    <div style={{ display: 'grid', gridTemplateColumns: '94px minmax(0, 1fr)', gap: 8, minHeight: 28, alignItems: 'center', fontSize: 11, borderTop: `1px solid ${t.border}` }}>
      <span style={{ color: t.muted }}>{label}</span>
      <span style={{ color: t.textSecondary, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>{value}</span>
    </div>
  )
}

function StartMain({ t }: { t: EguiTokens }) {
  return (
    <section style={{ minHeight: 0, display: 'grid', gridTemplateRows: '222px 40px minmax(0, 1fr) 112px', backgroundColor: t.input }}>
      <div style={{ borderBottom: `1px solid ${t.border}`, padding: 16 }}>
        <div style={{ color: t.text, fontSize: 22, fontWeight: 700 }}>개발 시작에 필요한 것들</div>
        <div style={{ marginTop: 8, color: t.textSecondary, fontSize: 12, lineHeight: 1.6 }}>
          이 화면은 내부 디버그가 아니라, 바로 일을 시작하기 위한 대시보드입니다. 가능한 스킬, 자주 쓰는 작업, 현재 프로젝트 상태를 한 곳에 모읍니다.
        </div>
        <div style={{ display: 'grid', gridTemplateColumns: 'repeat(3, 1fr)', gap: 10, marginTop: 16 }}>
          <MetricBox label="available skills" value="6" t={t} />
          <MetricBox label="quick actions" value="6" t={t} />
          <MetricBox label="current focus" value="UI" t={t} />
        </div>
      </div>
      <SectionHeader label="QUICK ACTIONS" t={t} action={<span style={{ color: t.muted, fontSize: 10 }}>frequent commands</span>} />
      <div style={{ minHeight: 0, overflowY: 'auto', padding: 12 }}>
        <div style={{ display: 'grid', gridTemplateColumns: 'repeat(2, minmax(0, 1fr))', gap: 10 }}>
          {QUICK_ACTIONS.map(([title, desc, tag]) => <QuickActionCard key={title} title={title} desc={desc} tag={tag} t={t} />)}
        </div>
      </div>
      <div style={{ borderTop: `1px solid ${t.border}`, backgroundColor: t.panel, padding: 10 }}>
        <div style={{ height: '100%', border: `1px solid ${t.inputBorder}`, backgroundColor: t.input, display: 'flex', alignItems: 'center', padding: '0 12px', color: t.text, fontSize: 13 }}>
          <span style={{ color: t.accent, marginRight: 8 }}>›</span>
          이 프로젝트에서 다음에 뭘 하면 좋을지 제안해줘
          <span style={{ width: 2, height: 18, backgroundColor: t.text, marginLeft: 4 }} />
        </div>
      </div>
    </section>
  )
}

function MetricBox({ label, value, t }: { label: string; value: string; t: EguiTokens }) {
  return (
    <div style={{ border: `1px solid ${t.border}`, backgroundColor: t.panel, padding: 10 }}>
      <div style={{ color: t.muted, fontSize: 10, letterSpacing: '0.06em' }}>{label}</div>
      <div style={{ marginTop: 8, color: t.accent, fontSize: 24, fontWeight: 700 }}>{value}</div>
    </div>
  )
}

function QuickActionCard({ title, desc, tag, t }: { title: string; desc: string; tag: string; t: EguiTokens }) {
  return (
    <div style={{ minHeight: 92, border: `1px solid ${t.border}`, backgroundColor: t.panel, padding: 12 }}>
      <div style={{ display: 'flex', alignItems: 'center', gap: 8 }}>
        <span style={{ color: t.text, fontSize: 14, fontWeight: 700 }}>{title}</span>
        <span style={{ marginLeft: 'auto', color: t.accent, border: `1px solid ${t.border}`, padding: '1px 5px', fontSize: 10 }}>{tag}</span>
      </div>
      <div style={{ marginTop: 9, color: t.textSecondary, fontSize: 12, lineHeight: 1.5 }}>{desc}</div>
    </div>
  )
}

function StartAssistantPanel({ t }: { t: EguiTokens }) {
  return (
    <aside style={{ minHeight: 0, display: 'grid', gridTemplateRows: '40px minmax(0, 1fr) 174px', borderLeft: `1px solid ${t.border}`, backgroundColor: t.panel }}>
      <SectionHeader label="SKILLS" t={t} action={<span style={{ color: t.muted, fontSize: 10 }}>available</span>} />
      <div style={{ minHeight: 0, overflowY: 'auto', padding: 10 }}>
        {START_SKILLS.map(([name, desc, state]) => <SkillRow key={name} name={name} desc={desc} state={state} t={t} />)}
      </div>
      <div style={{ borderTop: `1px solid ${t.border}`, padding: 10 }}>
        <div style={{ color: t.muted, fontSize: 10, letterSpacing: '0.08em', marginBottom: 8 }}>RECENT CONTEXT</div>
        {RECENT_CONTEXT.map(([label, value]) => <KeyValue key={label} label={label} value={value} t={t} />)}
        <div style={{ marginTop: 10, color: t.muted, fontSize: 10, lineHeight: 1.5 }}>
          실시간화 가능: skills registry, git status, dev server health, Codex event stream을 연결하면 됩니다.
        </div>
      </div>
    </aside>
  )
}

function SkillRow({ name, desc, state, t }: { name: string; desc: string; state: string; t: EguiTokens }) {
  const color = state === 'ready' ? t.tagText : state === 'guarded' ? '#c8a12d' : t.muted
  return (
    <div style={{ border: `1px solid ${t.border}`, backgroundColor: t.input, padding: 10, marginBottom: 8 }}>
      <div style={{ display: 'flex', alignItems: 'center', gap: 8 }}>
        <span style={{ color: t.accent, fontSize: 13, fontWeight: 700 }}>{name}</span>
        <span style={{ marginLeft: 'auto', color, fontSize: 10 }}>{state}</span>
      </div>
      <div style={{ marginTop: 7, color: t.textSecondary, fontSize: 11, lineHeight: 1.45 }}>{desc}</div>
    </div>
  )
}

function V3CommandBar({ t }: { t: EguiTokens }) {
  return (
    <div style={{
      display: 'grid',
      gridTemplateColumns: '336px minmax(0, 1fr) 318px',
      borderBottom: `1px solid ${t.border}`,
      backgroundColor: t.panel,
      color: t.textSecondary,
    }}>
      <div style={{
        display: 'flex',
        alignItems: 'center',
        gap: 8,
        padding: '0 12px',
        borderRight: `1px solid ${t.border}`,
      }}>
        <span style={{ color: t.accent, fontSize: 13 }}>◆</span>
        <span style={{ fontSize: 13, fontWeight: 700, color: t.text }}>Agent Terminal</span>
        <span style={{ marginLeft: 'auto', fontSize: 10, color: t.muted }}>v3 queue view</span>
      </div>
      <div style={{ display: 'flex', alignItems: 'center', gap: 12, padding: '0 14px', minWidth: 0 }}>
        <span style={{ fontSize: 11, color: t.muted }}>active task</span>
        <span style={{
          overflow: 'hidden',
          textOverflow: 'ellipsis',
          whiteSpace: 'nowrap',
          fontSize: 13,
          color: t.text,
        }}>
          T-104 · 전체 화면 v3 구성
        </span>
        <span style={{ marginLeft: 'auto', fontSize: 11, color: t.tagText }}>running</span>
      </div>
      <div style={{
        display: 'flex',
        alignItems: 'center',
        justifyContent: 'space-between',
        padding: '0 10px',
        borderLeft: `1px solid ${t.border}`,
        fontSize: 11,
      }}>
        <span style={{ color: t.muted }}>runtime</span>
        <span style={{ color: t.textSecondary }}>mock data · event-ready</span>
      </div>
    </div>
  )
}

function TaskQueuePanel({ t }: { t: EguiTokens }) {
  return (
    <aside style={{
      minHeight: 0,
      display: 'grid',
      gridTemplateRows: '40px minmax(0, 1fr) 152px',
      borderRight: `1px solid ${t.border}`,
      backgroundColor: t.panel,
    }}>
      <SectionHeader label="TASK QUEUE" t={t} action={<span style={{ fontSize: 10, color: t.muted }}>4 items</span>} />
      <div style={{ minHeight: 0, overflowY: 'auto', padding: 8 }}>
        {TASK_BOARD.map(task => <TaskCard key={task.id} task={task} t={t} />)}
      </div>
      <div style={{ borderTop: `1px solid ${t.border}`, padding: 10 }}>
        <div style={{ color: t.muted, fontSize: 10, letterSpacing: '0.08em', marginBottom: 8 }}>
          QUEUE CAN HOLD
        </div>
        <div style={{ display: 'grid', gridTemplateColumns: '1fr 1fr', gap: 6 }}>
          {QUEUE_CAPABILITIES.map(item => (
            <span key={item} style={{
              border: `1px solid ${t.border}`,
              backgroundColor: t.input,
              color: t.textSecondary,
              fontSize: 10,
              padding: '4px 6px',
              overflow: 'hidden',
              textOverflow: 'ellipsis',
              whiteSpace: 'nowrap',
            }}>
              {item}
            </span>
          ))}
        </div>
      </div>
    </aside>
  )
}

function TaskCard({ task, t }: { task: typeof TASK_BOARD[number]; t: EguiTokens }) {
  const active = task.status === 'active'
  const statusColor = task.status === 'blocked'
    ? t.danger
    : task.status === 'review'
      ? '#c8a12d'
      : task.status === 'active'
        ? t.accent
        : t.muted

  return (
    <div style={{
      border: `1px solid ${active ? t.accent : t.border}`,
      backgroundColor: active ? t.navActive : t.input,
      marginBottom: 8,
      padding: 10,
    }}>
      <div style={{ display: 'flex', alignItems: 'center', gap: 8 }}>
        <span style={{ color: statusColor, fontSize: 11, fontWeight: 700 }}>{task.id}</span>
        <span style={{ marginLeft: 'auto', color: statusColor, fontSize: 10 }}>{task.status}</span>
      </div>
      <div style={{ marginTop: 7, color: active ? t.navActiveText : t.text, fontSize: 13, fontWeight: 700 }}>
        {task.title}
      </div>
      <div style={{ marginTop: 5, color: t.textSecondary, fontSize: 11, lineHeight: 1.45 }}>
        {task.detail}
      </div>
      <div style={{ marginTop: 9, display: 'grid', gridTemplateColumns: '1fr 48px', gap: 8, alignItems: 'center' }}>
        <div style={{ height: 5, backgroundColor: t.surface, border: `1px solid ${t.border}` }}>
          <div style={{ width: `${task.progress}%`, height: '100%', backgroundColor: statusColor }} />
        </div>
        <span style={{ color: t.muted, fontSize: 10, textAlign: 'right' }}>{task.progress}%</span>
      </div>
    </div>
  )
}

function TaskExecutionPanel({ t }: { t: EguiTokens }) {
  return (
    <section style={{
      minHeight: 0,
      display: 'grid',
      gridTemplateRows: '112px minmax(0, 1fr) 122px',
      backgroundColor: t.input,
    }}>
      <div style={{ borderBottom: `1px solid ${t.border}`, padding: 16 }}>
        <div style={{ display: 'flex', alignItems: 'start', gap: 12 }}>
          <div style={{
            width: 48,
            height: 48,
            display: 'flex',
            alignItems: 'center',
            justifyContent: 'center',
            border: `1px solid ${t.accent}`,
            color: t.accent,
            fontSize: 18,
            fontWeight: 700,
          }}>
            T
          </div>
          <div style={{ minWidth: 0, flex: 1 }}>
            <div style={{ color: t.text, fontSize: 20, fontWeight: 700 }}>전체 화면 v3 구성</div>
            <div style={{ marginTop: 6, color: t.textSecondary, fontSize: 12, lineHeight: 1.5 }}>
              queue를 화면 중심에 두고, 선택 task의 실행 경로와 파일 영향도를 같이 보여주는 에이전트 터미널.
            </div>
          </div>
          <StatusBadge status="running" t={t} />
        </div>
      </div>

      <div style={{ minHeight: 0, overflowY: 'auto', padding: '8px 16px 16px' }}>
        {TASK_TIMELINE.map(([time, kind, title, detail]) => (
          <TimelineRow key={`${time}-${kind}`} time={time} kind={kind} title={title} detail={detail} t={t} />
        ))}
      </div>

      <div style={{ borderTop: `1px solid ${t.border}`, backgroundColor: t.panel, padding: 10 }}>
        <div style={{
          height: '100%',
          border: `1px solid ${t.inputBorder}`,
          backgroundColor: t.input,
          display: 'grid',
          gridTemplateRows: '1fr 26px',
        }}>
          <div style={{ display: 'flex', alignItems: 'center', padding: '0 12px', color: t.text, fontSize: 13 }}>
            <span style={{ color: t.accent, marginRight: 8 }}>›</span>
            task queue를 실제 이벤트 모델에 맞게 설계해줘
            <span style={{ width: 2, height: 18, backgroundColor: t.text, marginLeft: 4 }} />
          </div>
          <div style={{ borderTop: `1px solid ${t.border}`, display: 'flex', alignItems: 'center', padding: '0 10px', color: t.muted, fontSize: 10 }}>
            <span>queue: selected task</span>
            <span style={{ marginLeft: 'auto' }}>submit updates current task timeline</span>
          </div>
        </div>
      </div>
    </section>
  )
}

function TimelineRow({ time, kind, title, detail, t }: {
  time: string
  kind: string
  title: string
  detail: string
  t: EguiTokens
}) {
  const color = kind === 'edit' ? t.accent : kind === 'verify' ? t.tagText : kind === 'input' ? t.text : t.textSecondary

  return (
    <div style={{
      display: 'grid',
      gridTemplateColumns: '72px 72px minmax(0, 1fr)',
      gap: 10,
      padding: '10px 0',
      borderBottom: `1px solid ${t.border}`,
      fontSize: 12,
      lineHeight: 1.5,
    }}>
      <span style={{ color: t.muted, fontVariantNumeric: 'tabular-nums' }}>{time}</span>
      <span style={{ color, border: `1px solid ${t.border}`, textAlign: 'center', padding: '1px 0', fontSize: 10 }}>
        {kind}
      </span>
      <span style={{ minWidth: 0 }}>
        <span style={{ color: t.text, fontWeight: 700 }}>{title}</span>
        <span style={{ color: t.muted }}> · </span>
        <span style={{ color: t.textSecondary }}>{detail}</span>
      </span>
    </div>
  )
}

function TaskContextPanel({ t }: { t: EguiTokens }) {
  return (
    <aside style={{
      minHeight: 0,
      display: 'grid',
      gridTemplateRows: '40px 174px minmax(0, 1fr) 154px',
      borderLeft: `1px solid ${t.border}`,
      backgroundColor: t.panel,
    }}>
      <SectionHeader label="TASK CONTEXT" t={t} />
      <div style={{ padding: 10, borderBottom: `1px solid ${t.border}` }}>
        <KeyValue label="task" value="T-104" t={t} />
        <KeyValue label="agent" value="Design" t={t} />
        <KeyValue label="model" value="gpt-5.5 high" t={t} />
        <KeyValue label="mode" value="workspace-write" t={t} />
        <KeyValue label="server" value="localhost:8443" t={t} />
      </div>
      <div style={{ minHeight: 0, overflowY: 'auto' }}>
        <SectionHeader label="IMPACT FILES" t={t} />
        <div style={{ padding: '0 10px 10px' }}>
          {IMPACT_FILES.map(([state, file, note]) => (
            <ImpactRow key={file} state={state} file={file} note={note} t={t} />
          ))}
        </div>
        <SectionHeader label="QUEUE MODEL" t={t} />
        <div style={{ padding: '0 10px 10px', color: t.textSecondary, fontSize: 11, lineHeight: 1.6 }}>
          Queue는 stdout 줄이 아니라 작업 객체입니다. 실제 연동 시 `id`, `status`, `owner`, `steps`, `files`, `result`를 이벤트로 갱신합니다.
        </div>
      </div>
      <div style={{ borderTop: `1px solid ${t.border}`, padding: 10 }}>
        <div style={{ display: 'grid', gridTemplateColumns: '1fr 1fr', gap: 6 }}>
          {['pause task', 'cancel', 'promote', 'archive'].map(label => (
            <button key={label} style={{
              height: 30,
              backgroundColor: t.input,
              border: `1px solid ${t.border}`,
              color: t.textSecondary,
              fontFamily: 'inherit',
              fontSize: 10,
            }}>
              {label}
            </button>
          ))}
        </div>
        <div style={{ marginTop: 10, color: t.muted, fontSize: 10, lineHeight: 1.5 }}>
          v3 keeps v1/v2 intact and treats queue as first-class UI state.
        </div>
      </div>
    </aside>
  )
}

function ImpactRow({ state, file, note, t }: { state: string; file: string; note: string; t: EguiTokens }) {
  const color = state === 'new' ? t.tagText : state === 'modified' ? '#c8a12d' : t.muted
  return (
    <div style={{ borderTop: `1px solid ${t.border}`, padding: '8px 0' }}>
      <div style={{ display: 'flex', alignItems: 'center', gap: 8 }}>
        <span style={{ color, fontSize: 10, width: 48 }}>{state}</span>
        <span style={{
          minWidth: 0,
          flex: 1,
          color: t.text,
          fontSize: 11,
          overflow: 'hidden',
          textOverflow: 'ellipsis',
          whiteSpace: 'nowrap',
        }}>
          {file}
        </span>
      </div>
      <div style={{ marginTop: 4, color: t.muted, fontSize: 10, lineHeight: 1.4 }}>{note}</div>
    </div>
  )
}

function AgentRail({ t }: { t: EguiTokens }) {
  return (
    <aside style={{
      minHeight: 0,
      display: 'flex',
      flexDirection: 'column',
      borderRight: `1px solid ${t.border}`,
      backgroundColor: t.panel,
    }}>
      <SectionHeader label="AGENTS" t={t} action={<IconButton label="+" t={t} />} />
      <div style={{ padding: 8, borderBottom: `1px solid ${t.border}`, flexShrink: 0 }}>
        {SESSIONS.map(session => <CompactAgentItem key={session.id} session={session} t={t} />)}
      </div>

      <SectionHeader label="WORKSPACE" t={t} />
      <div style={{ padding: 10, borderBottom: `1px solid ${t.border}`, flexShrink: 0 }}>
        <KeyValue label="root" value="~/Desktop/Projects/deppy-sijo/design/환경설정 메뉴 구성" t={t} />
        <KeyValue label="branch" value="design/session-terminal" t={t} />
      </div>

      <SectionHeader label="FILES" t={t} action={<span style={{ color: t.muted, fontSize: 10 }}>2 modified</span>} />
      <div style={{
        flex: 1,
        minHeight: 0,
        overflowY: 'auto',
        padding: '6px 0',
        borderBottom: `1px solid ${t.border}`,
      }}>
        {FILE_TREE.map(node => <TreeRow key={node.name} node={node} depth={0} t={t} />)}
      </div>

      <SectionHeader label="TASK QUEUE" t={t} action={<span style={{ color: t.muted, fontSize: 10 }}>runtime data</span>} />
      <div style={{ flexShrink: 0, maxHeight: 132, overflowY: 'auto', padding: 8 }}>
        {QUEUE_ROWS.map(([state, name, model]) => (
          <QueueRow key={name} state={state} name={name} model={model} t={t} />
        ))}
      </div>
    </aside>
  )
}

function TreeRow({ node, depth, t }: { node: TreeNode; depth: number; t: EguiTokens }) {
  const selected = node.selected
  const isFolder = node.type === 'folder'
  const stateColor = node.state === 'modified' ? '#c8a12d' : node.state === 'new' ? t.tagText : t.muted

  return (
    <>
      <div style={{
        height: 25,
        display: 'grid',
        gridTemplateColumns: '16px 16px minmax(0, 1fr) 16px',
        alignItems: 'center',
        gap: 4,
        paddingLeft: 8 + depth * 14,
        paddingRight: 8,
        boxSizing: 'border-box',
        backgroundColor: selected ? t.navActive : 'transparent',
        color: selected ? t.navActiveText : t.textSecondary,
        fontSize: 11,
      }}>
        <span style={{ color: t.muted, fontSize: 10 }}>{isFolder ? '▾' : ''}</span>
        <span style={{ color: isFolder ? t.muted : t.textSecondary, fontSize: 12 }}>{isFolder ? '▤' : '□'}</span>
        <span style={{
          overflow: 'hidden',
          textOverflow: 'ellipsis',
          whiteSpace: 'nowrap',
          fontWeight: selected ? 700 : 400,
        }}>
          {node.name}
        </span>
        {node.state && node.state !== 'clean' && (
          <span style={{ color: stateColor, fontSize: 10, textAlign: 'right' }}>
            {node.state === 'modified' ? 'M' : 'N'}
          </span>
        )}
      </div>
      {node.children?.map(child => (
        <TreeRow key={`${node.name}/${child.name}`} node={child} depth={depth + 1} t={t} />
      ))}
    </>
  )
}

function CompactAgentItem({ session, t }: { session: Session; t: EguiTokens }) {
  const active = session.selected
  return (
    <div style={{
      display: 'grid',
      gridTemplateColumns: '4px minmax(0, 1fr)',
      minHeight: 62,
      marginBottom: 6,
      backgroundColor: active ? t.navActive : 'transparent',
      border: `1px solid ${active ? t.navActive : t.border}`,
    }}>
      <div style={{ backgroundColor: session.accent }} />
      <div style={{ minWidth: 0, padding: '7px 9px' }}>
        <div style={{ display: 'flex', alignItems: 'center', gap: 6 }}>
          <span style={{
            flex: 1,
            minWidth: 0,
            overflow: 'hidden',
            textOverflow: 'ellipsis',
            whiteSpace: 'nowrap',
            color: active ? t.navActiveText : t.textSecondary,
            fontSize: 14,
            fontWeight: active ? 700 : 500,
          }}>
            {session.name}
          </span>
          <StatusBadge status={session.status} t={t} />
        </div>
        <div style={{
          marginTop: 4,
          fontSize: 10,
          color: t.muted,
          overflow: 'hidden',
          textOverflow: 'ellipsis',
          whiteSpace: 'nowrap',
        }}>
          {session.model}
        </div>
        <div style={{ marginTop: 3, fontSize: 10, color: t.muted }}>
          {session.status === 'running' ? '실행 중' : '유휴'}
        </div>
      </div>
    </div>
  )
}

function KeyValue({ label, value, t }: { label: string; value: string; t: EguiTokens }) {
  return (
    <div style={{
      display: 'grid',
      gridTemplateColumns: '58px minmax(0, 1fr)',
      gap: 8,
      fontSize: 11,
      lineHeight: 1.8,
    }}>
      <span style={{ color: t.muted }}>{label}</span>
      <span style={{
        color: t.textSecondary,
        overflow: 'hidden',
        textOverflow: 'ellipsis',
        whiteSpace: 'nowrap',
      }}>
        {value}
      </span>
    </div>
  )
}

function QueueRow({ state, name, model, t }: { state: string; name: string; model: string; t: EguiTokens }) {
  const active = state === 'active'
  return (
    <div style={{
      border: `1px solid ${t.border}`,
      backgroundColor: active ? t.navActive : t.input,
      padding: '7px 8px',
      marginBottom: 6,
    }}>
      <div style={{ display: 'flex', alignItems: 'center', gap: 6 }}>
        <span style={{
          width: 7,
          height: 7,
          backgroundColor: state === 'running' ? t.accent : active ? t.tagText : t.muted,
        }} />
        <span style={{ flex: 1, fontSize: 12, color: active ? t.navActiveText : t.textSecondary }}>{name}</span>
        <span style={{ fontSize: 9, color: t.muted }}>{state}</span>
      </div>
      <div style={{ marginTop: 4, fontSize: 10, color: t.muted }}>{model}</div>
    </div>
  )
}

function AgentTerminal({ t }: { t: EguiTokens }) {
  return (
    <section style={{
      minWidth: 0,
      minHeight: 0,
      display: 'grid',
      gridTemplateRows: '40px 34px minmax(0, 1fr) 116px',
      backgroundColor: t.input,
    }}>
      <TerminalTabBar t={t} />
      <RunStatusBar t={t} />
      <AgentEventLog t={t} />
      <Composer t={t} />
    </section>
  )
}

function TerminalTabBar({ t }: { t: EguiTokens }) {
  return (
    <div style={{
      display: 'flex',
      alignItems: 'center',
      borderBottom: `1px solid ${t.border}`,
      backgroundColor: t.surface,
    }}>
      <div style={{
        height: '100%',
        display: 'flex',
        alignItems: 'center',
        gap: 8,
        padding: '0 14px',
        borderRight: `1px solid ${t.border}`,
        color: t.accent,
        fontSize: 13,
        fontWeight: 700,
      }}>
        <span style={{ color: t.tagText }}>◆</span>
        <span>Design</span>
        <span style={{ color: t.muted }}>×</span>
      </div>
      <span style={{ marginLeft: 12, color: t.muted, fontSize: 11 }}>
        agent terminal · cwd /Users · hooks enabled
      </span>
      <div style={{ marginLeft: 'auto', display: 'flex', gap: 8, paddingRight: 10 }}>
        <IconButton label="+" t={t} />
        <IconButton label="▣" t={t} />
        <IconButton label="↻" t={t} />
      </div>
    </div>
  )
}

function RunStatusBar({ t }: { t: EguiTokens }) {
  return (
    <div style={{
      display: 'grid',
      gridTemplateColumns: '1fr 1fr 1fr',
      borderBottom: `1px solid ${t.border}`,
      backgroundColor: t.panel,
      fontSize: 11,
      color: t.textSecondary,
    }}>
      <StatusCell label="model" value="gpt-5.5 high" t={t} />
      <StatusCell label="mode" value="workspace-write" t={t} />
      <StatusCell label="verify" value="pnpm build passed" t={t} last />
    </div>
  )
}

function StatusCell({ label, value, t, last }: { label: string; value: string; t: EguiTokens; last?: boolean }) {
  return (
    <div style={{
      display: 'flex',
      alignItems: 'center',
      gap: 8,
      padding: '0 12px',
      borderRight: last ? 'none' : `1px solid ${t.border}`,
      minWidth: 0,
    }}>
      <span style={{ color: t.muted }}>{label}</span>
      <span style={{ color: t.textSecondary, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>
        {value}
      </span>
    </div>
  )
}

function AgentEventLog({ t }: { t: EguiTokens }) {
  return (
    <div style={{ minHeight: 0, overflowY: 'auto', padding: 14 }}>
      {AGENT_EVENTS.map(event => <EventRow key={`${event.time}-${event.label}`} event={event} t={t} />)}
    </div>
  )
}

function EventRow({ event, t }: { event: typeof AGENT_EVENTS[number]; t: EguiTokens }) {
  const colorByKind: Record<string, string> = {
    system: t.muted,
    user: t.text,
    plan: t.tagText,
    tool: t.accent,
    assistant: t.textSecondary,
    warn: '#c8a12d',
    success: '#66c587',
  }

  return (
    <div style={{
      display: 'grid',
      gridTemplateColumns: '76px 76px minmax(0, 1fr)',
      gap: 10,
      alignItems: 'start',
      padding: '8px 0',
      borderBottom: `1px solid ${t.border}`,
      fontSize: 12,
      lineHeight: 1.55,
    }}>
      <span style={{ color: t.muted, fontVariantNumeric: 'tabular-nums' }}>{event.time}</span>
      <span style={{
        width: 58,
        textAlign: 'center',
        color: colorByKind[event.kind],
        border: `1px solid ${t.border}`,
        backgroundColor: event.kind === 'user' ? t.surface : t.input,
        fontSize: 10,
        padding: '1px 0',
      }}>
        {event.label}
      </span>
      <span style={{ minWidth: 0, color: colorByKind[event.kind], wordBreak: 'keep-all' }}>
        {event.text}
      </span>
    </div>
  )
}

function Composer({ t }: { t: EguiTokens }) {
  return (
    <div style={{
      borderTop: `1px solid ${t.border}`,
      backgroundColor: t.panel,
      padding: 10,
      boxSizing: 'border-box',
    }}>
      <div style={{
        height: '100%',
        display: 'grid',
        gridTemplateRows: '1fr 24px',
        border: `1px solid ${t.inputBorder}`,
        backgroundColor: t.input,
      }}>
        <div style={{
          display: 'flex',
          alignItems: 'center',
          padding: '0 12px',
          color: t.text,
          fontSize: 13,
        }}>
          <span style={{ color: t.accent, marginRight: 8 }}>›</span>
          전체 화면 UI를 egui 토큰 안에서 구현해줘
          <span style={{ width: 2, height: 18, backgroundColor: t.text, marginLeft: 4 }} />
        </div>
        <div style={{
          display: 'flex',
          alignItems: 'center',
          gap: 10,
          padding: '0 10px',
          borderTop: `1px solid ${t.border}`,
          color: t.muted,
          fontSize: 10,
        }}>
          <span>/model</span>
          <span>/usage</span>
          <span>/compact</span>
          <span style={{ marginLeft: 'auto', color: t.textSecondary }}>enter: send · shift enter: newline</span>
        </div>
      </div>
    </div>
  )
}

function AgentInspector({ t }: { t: EguiTokens }) {
  return (
    <aside style={{
      minHeight: 0,
      display: 'flex',
      flexDirection: 'column',
      borderLeft: `1px solid ${t.border}`,
      backgroundColor: t.panel,
    }}>
      <SectionHeader label="INSPECTOR" t={t} />
      <div style={{ padding: 10, borderBottom: `1px solid ${t.border}` }}>
        <div style={{ fontSize: 18, color: t.text, fontWeight: 700 }}>Design</div>
        <div style={{ marginTop: 4, fontSize: 11, color: t.muted }}>selected pane context · live when runtime is connected</div>
      </div>
      <InspectorBlock title="TOOLS" t={t}>
        {TOOL_ROWS.map(([name, detail, state]) => (
          <ToolRow key={name} name={name} detail={detail} state={state} t={t} />
        ))}
      </InspectorBlock>
      <InspectorBlock title="CONTEXT" t={t}>
        <KeyValue label="selected" value="src/components/SessionWorkspace.tsx" t={t} />
        <KeyValue label="changed" value="App.tsx, SessionWorkspace.tsx" t={t} />
        <KeyValue label="style" value="1px border · mono · no gradient" t={t} />
        <KeyValue label="layout" value="tree / terminal / inspector" t={t} />
      </InspectorBlock>
      <InspectorBlock title="RUN CONTROLS" t={t}>
        <div style={{ display: 'grid', gridTemplateColumns: '1fr 1fr', gap: 6 }}>
          {['pause', 'stop', 'fork', 'copy'].map(label => (
            <button key={label} style={{
              height: 28,
              backgroundColor: t.input,
              border: `1px solid ${t.border}`,
              color: t.textSecondary,
              fontFamily: 'inherit',
              fontSize: 11,
            }}>
              {label}
            </button>
          ))}
        </div>
      </InspectorBlock>
      <div style={{ flex: 1 }} />
      <div style={{
        padding: 10,
        borderTop: `1px solid ${t.border}`,
        color: t.muted,
        fontSize: 10,
        lineHeight: 1.6,
      }}>
        Inspector purpose: selected file, tool state, and action controls. Queue/output need Codex event data, not static terminal text.
      </div>
    </aside>
  )
}

function InspectorBlock({ title, t, children }: { title: string; t: EguiTokens; children: ReactNode }) {
  return (
    <div style={{ borderBottom: `1px solid ${t.border}` }}>
      <div style={{
        height: 30,
        display: 'flex',
        alignItems: 'center',
        padding: '0 10px',
        color: t.muted,
        fontSize: 10,
        letterSpacing: '0.08em',
      }}>
        {title}
      </div>
      <div style={{ padding: '0 10px 10px' }}>{children}</div>
    </div>
  )
}

function ToolRow({ name, detail, state, t }: { name: string; detail: string; state: string; t: EguiTokens }) {
  return (
    <div style={{
      display: 'grid',
      gridTemplateColumns: '42px minmax(0, 1fr) 42px',
      gap: 6,
      alignItems: 'center',
      minHeight: 26,
      fontSize: 10,
      borderTop: `1px solid ${t.border}`,
    }}>
      <span style={{ color: t.accent }}>{name}</span>
      <span style={{ color: t.textSecondary, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>{detail}</span>
      <span style={{ color: state === 'pass' || state === 'ready' ? t.tagText : t.muted, textAlign: 'right' }}>{state}</span>
    </div>
  )
}

function ScenariosPage({ t }: { t: EguiTokens }) {
  const [activeScenario, setActiveScenario] = useState(USAGE_SCENARIOS[0].id)
  const scenario = USAGE_SCENARIOS.find(item => item.id === activeScenario) ?? USAGE_SCENARIOS[0]

  return (
    <WindowFrame t={t} width={1380} height={820}>
      <div style={{
        height: '100%',
        minWidth: 0,
        display: 'grid',
        gridTemplateRows: '48px minmax(0, 1fr)',
        backgroundColor: t.input,
      }}>
        <ScenarioHeader t={t} />
        <div style={{
          minHeight: 0,
          display: 'grid',
          gridTemplateColumns: '300px minmax(0, 1fr) 340px',
        }}>
          <ScenarioTabs activeId={activeScenario} onChange={setActiveScenario} t={t} />
          <ScenarioFlow scenario={scenario} t={t} />
          <ScenarioContext scenario={scenario} t={t} />
        </div>
      </div>
    </WindowFrame>
  )
}

function ScenarioHeader({ t }: { t: EguiTokens }) {
  return (
    <div style={{
      display: 'grid',
      gridTemplateColumns: '300px minmax(0, 1fr) 340px',
      borderBottom: `1px solid ${t.border}`,
      backgroundColor: t.panel,
    }}>
      <div style={{ display: 'flex', alignItems: 'center', gap: 8, padding: '0 12px', borderRight: `1px solid ${t.border}` }}>
        <span style={{ color: t.accent }}>◆</span>
        <span style={{ color: t.text, fontSize: 14, fontWeight: 700 }}>Scenario Tabs</span>
      </div>
      <div style={{ display: 'flex', alignItems: 'center', padding: '0 14px', gap: 12 }}>
        <span style={{ color: t.muted, fontSize: 11 }}>how to use</span>
        <span style={{ color: t.text, fontSize: 14, fontWeight: 700 }}>개발 시작 대시보드를 실제 작업 흐름으로 쓰는 방법</span>
      </div>
      <div style={{ display: 'flex', alignItems: 'center', padding: '0 12px', borderLeft: `1px solid ${t.border}` }}>
        <span style={{ color: t.muted, fontSize: 11 }}>html scenario mock</span>
        <span style={{ marginLeft: 'auto', color: t.textSecondary, fontSize: 11 }}>2 flows</span>
      </div>
    </div>
  )
}

function ScenarioTabs({
  activeId,
  onChange,
  t,
}: {
  activeId: string
  onChange: (id: string) => void
  t: EguiTokens
}) {
  return (
    <aside style={{
      minHeight: 0,
      display: 'grid',
      gridTemplateRows: '40px minmax(0, 1fr) 160px',
      borderRight: `1px solid ${t.border}`,
      backgroundColor: t.panel,
    }}>
      <SectionHeader label="SCENARIOS" t={t} />
      <div style={{ minHeight: 0, overflowY: 'auto', padding: 8 }}>
        {USAGE_SCENARIOS.map((item, index) => {
          const active = item.id === activeId
          return (
            <button
              key={item.id}
              onClick={() => onChange(item.id)}
              style={{
                display: 'block',
                width: '100%',
                minHeight: 96,
                marginBottom: 8,
                textAlign: 'left',
                backgroundColor: active ? t.navActive : t.input,
                border: `1px solid ${active ? t.accent : t.border}`,
                color: active ? t.navActiveText : t.textSecondary,
                fontFamily: 'inherit',
                padding: 10,
                cursor: 'pointer',
              }}
            >
              <div style={{ color: active ? t.accent : t.muted, fontSize: 10 }}>SCENARIO {index + 1}</div>
              <div style={{ marginTop: 7, color: active ? t.navActiveText : t.text, fontSize: 13, fontWeight: 700 }}>
                {item.title.replace(/^시나리오 \d · /, '')}
              </div>
              <div style={{ marginTop: 6, color: t.muted, fontSize: 11, lineHeight: 1.45 }}>
                {item.goal}
              </div>
            </button>
          )
        })}
      </div>
      <div style={{ borderTop: `1px solid ${t.border}`, padding: 10 }}>
        <div style={{ color: t.muted, fontSize: 10, letterSpacing: '0.08em', marginBottom: 8 }}>USAGE RULE</div>
        <div style={{ color: t.textSecondary, fontSize: 11, lineHeight: 1.55 }}>
          시나리오는 사용자가 보는 화면 순서입니다. 실제 연동 시 각 단계는 task/event 객체로 기록됩니다.
        </div>
      </div>
    </aside>
  )
}

function ScenarioFlow({ scenario, t }: { scenario: typeof USAGE_SCENARIOS[number]; t: EguiTokens }) {
  return (
    <section style={{
      minHeight: 0,
      display: 'grid',
      gridTemplateRows: '156px 40px minmax(0, 1fr) 118px',
      backgroundColor: t.input,
    }}>
      <div style={{ borderBottom: `1px solid ${t.border}`, padding: 16 }}>
        <div style={{ color: t.text, fontSize: 22, fontWeight: 700 }}>{scenario.title}</div>
        <div style={{ marginTop: 8, color: t.textSecondary, fontSize: 12, lineHeight: 1.6 }}>{scenario.goal}</div>
        <div style={{
          marginTop: 14,
          border: `1px solid ${t.inputBorder}`,
          backgroundColor: t.panel,
          padding: '10px 12px',
          color: t.text,
          fontSize: 13,
        }}>
          <span style={{ color: t.accent, marginRight: 8 }}>›</span>
          {scenario.prompt}
        </div>
      </div>
      <SectionHeader label="FLOW STEPS" t={t} action={<span style={{ color: t.muted, fontSize: 10 }}>{scenario.steps.length} steps</span>} />
      <div style={{ minHeight: 0, overflowY: 'auto', padding: '4px 16px 16px' }}>
        {scenario.steps.map(([number, title, detail]) => (
          <ScenarioStep key={number} number={number} title={title} detail={detail} t={t} />
        ))}
      </div>
      <div style={{ borderTop: `1px solid ${t.border}`, backgroundColor: t.panel, padding: 10 }}>
        <div style={{ height: '100%', border: `1px solid ${t.inputBorder}`, backgroundColor: t.input, display: 'flex', alignItems: 'center', padding: '0 12px', color: t.text, fontSize: 13 }}>
          <span style={{ color: t.accent, marginRight: 8 }}>›</span>
          이 시나리오대로 진행해줘
          <span style={{ width: 2, height: 18, backgroundColor: t.text, marginLeft: 4 }} />
        </div>
      </div>
    </section>
  )
}

function ScenarioStep({ number, title, detail, t }: {
  number: string
  title: string
  detail: string
  t: EguiTokens
}) {
  return (
    <div style={{
      display: 'grid',
      gridTemplateColumns: '44px 132px minmax(0, 1fr)',
      gap: 12,
      alignItems: 'start',
      padding: '13px 0',
      borderBottom: `1px solid ${t.border}`,
      fontSize: 12,
      lineHeight: 1.55,
    }}>
      <span style={{
        width: 28,
        height: 28,
        display: 'flex',
        alignItems: 'center',
        justifyContent: 'center',
        border: `1px solid ${t.accent}`,
        color: t.accent,
        fontWeight: 700,
      }}>
        {number}
      </span>
      <span style={{ color: t.text, fontWeight: 700 }}>{title}</span>
      <span style={{ color: t.textSecondary, wordBreak: 'keep-all' }}>{detail}</span>
    </div>
  )
}

function ScenarioContext({ scenario, t }: { scenario: typeof USAGE_SCENARIOS[number]; t: EguiTokens }) {
  return (
    <aside style={{
      minHeight: 0,
      display: 'grid',
      gridTemplateRows: '40px 170px 40px minmax(0, 1fr) 142px',
      borderLeft: `1px solid ${t.border}`,
      backgroundColor: t.panel,
    }}>
      <SectionHeader label="SCENARIO CONTEXT" t={t} />
      <div style={{ padding: 10, borderBottom: `1px solid ${t.border}` }}>
        <KeyValue label="skill" value={scenario.skill} t={t} />
        <KeyValue label="source" value="사용자 요청 + 프로젝트 상태" t={t} />
        <KeyValue label="queue" value="task 객체로 기록" t={t} />
        <KeyValue label="verify" value="build/browser result" t={t} />
      </div>
      <SectionHeader label="SCREEN AREAS USED" t={t} />
      <div style={{ minHeight: 0, overflowY: 'auto', padding: 10 }}>
        {[
          ['Start Checklist', '서버/빌드/entry 상태를 먼저 확인'],
          ['Project Tree', '작업 대상 파일 위치와 변경 상태 확인'],
          ['Quick Actions', '자주 쓰는 명령을 바로 실행'],
          ['Skills', '작업에 필요한 능력과 권한 확인'],
          ['Task Queue', '현재 작업과 대기 작업의 상태 관리'],
        ].map(([label, desc]) => (
          <div key={label} style={{ border: `1px solid ${t.border}`, backgroundColor: t.input, padding: 9, marginBottom: 8 }}>
            <div style={{ color: t.accent, fontSize: 11, fontWeight: 700 }}>{label}</div>
            <div style={{ marginTop: 5, color: t.textSecondary, fontSize: 11, lineHeight: 1.45 }}>{desc}</div>
          </div>
        ))}
      </div>
      <div style={{ borderTop: `1px solid ${t.border}`, padding: 10 }}>
        <div style={{ color: t.muted, fontSize: 10, letterSpacing: '0.08em', marginBottom: 8 }}>EXPECTED RESULT</div>
        <div style={{ color: t.textSecondary, fontSize: 11, lineHeight: 1.6 }}>{scenario.result}</div>
      </div>
    </aside>
  )
}

function DeppyScenariosPage({ t }: { t: EguiTokens }) {
  const [activeScenario, setActiveScenario] = useState(DEPPY_SCENARIOS[0].id)
  const scenario = DEPPY_SCENARIOS.find(item => item.id === activeScenario) ?? DEPPY_SCENARIOS[0]

  return (
    <WindowFrame t={t} width={1420} height={840}>
      <div style={{
        height: '100%',
        minWidth: 0,
        display: 'grid',
        gridTemplateRows: '50px minmax(0, 1fr)',
        backgroundColor: t.input,
      }}>
        <DeppyHeader t={t} />
        <div style={{
          minHeight: 0,
          display: 'grid',
          gridTemplateColumns: '318px minmax(0, 1fr) 380px',
        }}>
          <DeppySetupPanel t={t} />
          <DeppyScenarioPanel scenario={scenario} activeId={activeScenario} onChange={setActiveScenario} t={t} />
          <DeppyRuntimePanel t={t} />
        </div>
      </div>
    </WindowFrame>
  )
}

function DeppyHeader({ t }: { t: EguiTokens }) {
  return (
    <div style={{
      display: 'grid',
      gridTemplateColumns: '318px minmax(0, 1fr) 380px',
      borderBottom: `1px solid ${t.border}`,
      backgroundColor: t.panel,
    }}>
      <div style={{ display: 'flex', alignItems: 'center', gap: 8, padding: '0 12px', borderRight: `1px solid ${t.border}` }}>
        <span style={{ color: t.accent }}>◆</span>
        <span style={{ color: t.text, fontSize: 14, fontWeight: 700 }}>Deppy Remote Ops</span>
      </div>
      <div style={{ display: 'flex', alignItems: 'center', gap: 12, padding: '0 14px', minWidth: 0 }}>
        <span style={{ color: t.muted, fontSize: 11 }}>review result</span>
        <span style={{ color: t.text, fontSize: 14, fontWeight: 700, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>
          로그인 · 커넥터 · 예약 실행 · 모바일 승인으로 일상 에이전트 운영
        </span>
      </div>
      <div style={{ display: 'flex', alignItems: 'center', padding: '0 12px', borderLeft: `1px solid ${t.border}` }}>
        <span style={{ color: t.muted, fontSize: 11 }}>computer off</span>
        <span style={{ marginLeft: 'auto', color: t.tagText, fontSize: 12, fontWeight: 700 }}>cloud run OK</span>
      </div>
    </div>
  )
}

function DeppySetupPanel({ t }: { t: EguiTokens }) {
  return (
    <aside style={{
      minHeight: 0,
      display: 'grid',
      gridTemplateRows: '40px 250px 40px minmax(0, 1fr) 154px',
      borderRight: `1px solid ${t.border}`,
      backgroundColor: t.panel,
    }}>
      <SectionHeader label="LINK SETUP" t={t} action={<span style={{ color: t.tagText, fontSize: 10 }}>required</span>} />
      <div style={{ padding: 10, borderBottom: `1px solid ${t.border}` }}>
        {DEPPY_SETUP.map(([key, label, detail]) => (
          <DeppySetupRow key={key} label={label} detail={detail} t={t} />
        ))}
      </div>
      <SectionHeader label="PROJECT SURFACE" t={t} />
      <div style={{ minHeight: 0, overflowY: 'auto', padding: 10 }}>
        {[
          ['apps/web', 'agents, device login, notifications, connections'],
          ['apps/api', 'agent run, subscription run, scheduled task APIs'],
          ['apps/worker', 'BullMQ processor, reports, notifications'],
          ['connection-broker', 'gmail, calendar, docs, drive, sheets, memory'],
          ['agent-control-plane', 'cloud run start, runtime deployment'],
          ['wake-proxy', 'scale-to-zero service wake path'],
        ].map(([label, detail]) => (
          <div key={label} style={{ border: `1px solid ${t.border}`, backgroundColor: t.input, padding: 9, marginBottom: 8 }}>
            <div style={{ color: t.accent, fontSize: 11, fontWeight: 700 }}>{label}</div>
            <div style={{ marginTop: 5, color: t.textSecondary, fontSize: 10, lineHeight: 1.45 }}>{detail}</div>
          </div>
        ))}
      </div>
      <div style={{ borderTop: `1px solid ${t.border}`, padding: 10 }}>
        <div style={{ color: t.muted, fontSize: 10, letterSpacing: '0.08em', marginBottom: 8 }}>PRODUCT RULE</div>
        <div style={{ color: t.textSecondary, fontSize: 11, lineHeight: 1.6 }}>
          사용자의 컴퓨터는 설정/개발용입니다. 반복 업무는 Deppy 서버의 예약 작업과 런타임에서 실행되어야 합니다.
        </div>
      </div>
    </aside>
  )
}

function DeppySetupRow({ label, detail, t }: { label: string; detail: string; t: EguiTokens }) {
  return (
    <div style={{
      minHeight: 34,
      display: 'grid',
      gridTemplateColumns: '84px minmax(0, 1fr)',
      gap: 8,
      alignItems: 'start',
      padding: '8px 0',
      borderTop: `1px solid ${t.border}`,
      fontSize: 11,
      lineHeight: 1.45,
    }}>
      <span style={{ color: t.text, fontWeight: 700 }}>{label}</span>
      <span style={{ color: t.textSecondary, wordBreak: 'keep-all' }}>{detail}</span>
    </div>
  )
}

function DeppyScenarioPanel({
  scenario,
  activeId,
  onChange,
  t,
}: {
  scenario: typeof DEPPY_SCENARIOS[number]
  activeId: string
  onChange: (id: string) => void
  t: EguiTokens
}) {
  return (
    <section style={{
      minHeight: 0,
      display: 'grid',
      gridTemplateRows: '132px 86px 40px minmax(0, 1fr) 112px',
      backgroundColor: t.input,
    }}>
      <div style={{ borderBottom: `1px solid ${t.border}`, padding: 16 }}>
        <div style={{ color: t.text, fontSize: 22, fontWeight: 700 }}>노트북 없이 굴러가는 생활 에이전트</div>
        <div style={{ marginTop: 8, color: t.textSecondary, fontSize: 12, lineHeight: 1.6, wordBreak: 'keep-all' }}>
          Deppy는 이미 로그인, 알림 채널, scheduled task, subscription run, pause/resume 모델을 갖고 있습니다.
          여기에 모바일 화면과 정책을 붙이면 “시켜두고 확인하는” 운영 제품으로 확장할 수 있습니다.
        </div>
      </div>
      <div style={{ display: 'grid', gridTemplateColumns: '1fr 1fr', gap: 8, padding: 10, borderBottom: `1px solid ${t.border}` }}>
        {DEPPY_SCENARIOS.map((item, index) => {
          const active = item.id === activeId
          return (
            <button
              key={item.id}
              onClick={() => onChange(item.id)}
              style={{
                textAlign: 'left',
                backgroundColor: active ? t.navActive : t.panel,
                border: `1px solid ${active ? t.accent : t.border}`,
                color: active ? t.navActiveText : t.textSecondary,
                fontFamily: 'inherit',
                padding: 10,
                cursor: 'pointer',
              }}
            >
              <div style={{ color: active ? t.accent : t.muted, fontSize: 10 }}>SCENARIO {index + 1}</div>
              <div style={{ marginTop: 5, color: active ? t.navActiveText : t.text, fontSize: 12, fontWeight: 700 }}>
                {item.title.replace(/^시나리오 \d · /, '')}
              </div>
            </button>
          )
        })}
      </div>
      <SectionHeader label="RUN FLOW" t={t} action={<span style={{ color: t.muted, fontSize: 10 }}>{scenario.trigger}</span>} />
      <div style={{ minHeight: 0, overflowY: 'auto', padding: '4px 16px 16px' }}>
        <div style={{ padding: '12px 0', borderBottom: `1px solid ${t.border}` }}>
          <div style={{ color: t.text, fontSize: 18, fontWeight: 700 }}>{scenario.title}</div>
          <div style={{ marginTop: 6, color: t.textSecondary, fontSize: 12, lineHeight: 1.55 }}>{scenario.promise}</div>
          <div style={{ marginTop: 8, display: 'flex', gap: 8 }}>
            <SmallTag label={scenario.trigger} t={t} />
            <SmallTag label={scenario.mobile} t={t} />
          </div>
        </div>
        {scenario.steps.map(([time, title, detail]) => (
          <DeppyRunStep key={`${time}-${title}`} time={time} title={title} detail={detail} t={t} />
        ))}
      </div>
      <div style={{ borderTop: `1px solid ${t.border}`, backgroundColor: t.panel, padding: 10 }}>
        <div style={{ height: '100%', border: `1px solid ${t.inputBorder}`, backgroundColor: t.input, display: 'flex', alignItems: 'center', padding: '0 12px', color: t.text, fontSize: 13 }}>
          <span style={{ color: t.accent, marginRight: 8 }}>›</span>
          {scenario.result}
        </div>
      </div>
    </section>
  )
}

function SmallTag({ label, t }: { label: string; t: EguiTokens }) {
  return (
    <span style={{
      border: `1px solid ${t.border}`,
      backgroundColor: t.panel,
      color: t.accent,
      fontSize: 10,
      padding: '3px 7px',
    }}>
      {label}
    </span>
  )
}

function DeppyRunStep({ time, title, detail, t }: {
  time: string
  title: string
  detail: string
  t: EguiTokens
}) {
  return (
    <div style={{
      display: 'grid',
      gridTemplateColumns: '58px 138px minmax(0, 1fr)',
      gap: 12,
      alignItems: 'start',
      padding: '12px 0',
      borderBottom: `1px solid ${t.border}`,
      fontSize: 12,
      lineHeight: 1.55,
    }}>
      <span style={{ color: t.muted, fontVariantNumeric: 'tabular-nums' }}>{time}</span>
      <span style={{ color: t.accent, fontWeight: 700 }}>{title}</span>
      <span style={{ color: t.textSecondary, wordBreak: 'keep-all' }}>{detail}</span>
    </div>
  )
}

function DeppyRuntimePanel({ t }: { t: EguiTokens }) {
  return (
    <aside style={{
      minHeight: 0,
      display: 'grid',
      gridTemplateRows: '40px 278px 40px minmax(0, 1fr) 154px',
      borderLeft: `1px solid ${t.border}`,
      backgroundColor: t.panel,
    }}>
      <SectionHeader label="MOBILE TERMINAL" t={t} action={<span style={{ color: t.tagText, fontSize: 10 }}>live-ready</span>} />
      <div style={{ padding: 10, borderBottom: `1px solid ${t.border}`, backgroundColor: t.input, overflow: 'hidden' }}>
        {DEPPY_TERMINAL.map(([kind, command, result]) => (
          <DeppyTerminalRow key={`${kind}-${command}`} kind={kind} command={command} result={result} t={t} />
        ))}
      </div>
      <SectionHeader label="WHAT IS REALISTIC" t={t} />
      <div style={{ minHeight: 0, overflowY: 'auto', padding: 10 }}>
        {DEPPY_BOUNDARIES.map(([state, label, detail]) => (
          <div key={label} style={{
            display: 'grid',
            gridTemplateColumns: '52px 86px minmax(0, 1fr)',
            gap: 8,
            padding: '9px 0',
            borderTop: `1px solid ${t.border}`,
            fontSize: 11,
            lineHeight: 1.45,
          }}>
            <span style={{ color: state === '가능' ? t.tagText : '#c8a12d', fontWeight: 700 }}>{state}</span>
            <span style={{ color: t.text }}>{label}</span>
            <span style={{ color: t.textSecondary, wordBreak: 'keep-all' }}>{detail}</span>
          </div>
        ))}
      </div>
      <div style={{ borderTop: `1px solid ${t.border}`, padding: 10 }}>
        <div style={{ color: t.muted, fontSize: 10, letterSpacing: '0.08em', marginBottom: 8 }}>NEXT PRODUCT STEP</div>
        <div style={{ color: t.textSecondary, fontSize: 11, lineHeight: 1.6 }}>
          모바일에서 run detail, approve/edit/reject, schedule template, notification deep link를 먼저 완성하면 일상 운영 흐름이 보입니다.
        </div>
      </div>
    </aside>
  )
}

function DeppyTerminalRow({ kind, command, result, t }: {
  kind: string
  command: string
  result: string
  t: EguiTokens
}) {
  const color = kind === 'pause' ? '#c8a12d' : kind === 'done' || kind === 'cloud' ? t.tagText : kind === 'local' ? t.text : t.accent
  return (
    <div style={{ padding: '7px 0', borderBottom: `1px solid ${t.border}`, fontSize: 11, lineHeight: 1.45 }}>
      <div style={{ display: 'grid', gridTemplateColumns: '54px minmax(0, 1fr)', gap: 8 }}>
        <span style={{ color, fontWeight: 700 }}>{kind}</span>
        <span style={{ color: t.text, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>{command}</span>
      </div>
      <div style={{ marginTop: 3, paddingLeft: 62, color: t.muted, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>
        {result}
      </div>
    </div>
  )
}

function FocusedPage({ title, t, children }: { title: string; t: EguiTokens; children: ReactNode }) {
  return (
    <WindowFrame t={t} width={860} height={560}>
      <div style={{ height: '100%', display: 'flex', flexDirection: 'column' }}>
        <HeaderBar title={title} t={t} right={<span style={{ color: t.muted, fontSize: 10 }}>component preview</span>} />
        <div style={{
          flex: 1,
          display: 'flex',
          alignItems: 'center',
          justifyContent: 'center',
          backgroundColor: t.surface,
          padding: 24,
        }}>
          {children}
        </div>
      </div>
    </WindowFrame>
  )
}

function WindowFrame({ t, width, height, children }: {
  t: EguiTokens
  width: number
  height: number
  children: ReactNode
}) {
  return (
    <div style={{
      width,
      height,
      maxWidth: '100%',
      maxHeight: 'calc(100vh - 90px)',
      border: `1px solid ${t.border}`,
      backgroundColor: t.surface,
      boxShadow: '0 8px 32px rgba(0,0,0,0.45)',
      overflow: 'hidden',
      display: 'flex',
      flexDirection: 'column',
    }}>
      <WindowChrome t={t} />
      <div style={{ flex: 1, minHeight: 0 }}>{children}</div>
    </div>
  )
}

function WindowChrome({ t }: { t: EguiTokens }) {
  return (
    <div style={{
      height: 34,
      display: 'flex',
      alignItems: 'center',
      gap: 10,
      padding: '0 10px',
      borderBottom: `1px solid ${t.border}`,
      backgroundColor: t.panel,
      flexShrink: 0,
      boxSizing: 'border-box',
    }}>
      <span style={{ width: 14, height: 14, borderRadius: 999, backgroundColor: '#ff5f57' }} />
      <span style={{ width: 14, height: 14, borderRadius: 999, backgroundColor: '#ffbd2e' }} />
      <span style={{ width: 14, height: 14, borderRadius: 999, backgroundColor: '#28c840' }} />
      <span style={{ marginLeft: 12, fontSize: 12, color: t.textSecondary }}>설정</span>
      <span style={{ marginLeft: 'auto', fontSize: 12, color: t.muted }}>ko · 103MB</span>
    </div>
  )
}

function HeaderBar({ title, right, t }: { title: string; right?: ReactNode; t: EguiTokens }) {
  return (
    <div style={{
      height: 36,
      borderBottom: `1px solid ${t.border}`,
      display: 'flex',
      alignItems: 'center',
      justifyContent: 'space-between',
      padding: '0 16px',
      flexShrink: 0,
      backgroundColor: t.surface,
      boxSizing: 'border-box',
    }}>
      <span style={{ fontSize: 13, fontWeight: 600, color: t.text, letterSpacing: '0.02em' }}>{title}</span>
      {right}
    </div>
  )
}

function SessionSidebar({ t }: { t: EguiTokens }) {
  return (
    <aside style={{
      width: 386,
      borderRight: `1px solid ${t.border}`,
      display: 'flex',
      flexDirection: 'column',
      flexShrink: 0,
      backgroundColor: t.panel,
      minHeight: 0,
    }}>
      <SectionHeader label="세션" t={t} action={<IconButton label="+" t={t} />} />
      <div style={{ padding: '6px 8px 10px', borderBottom: `1px solid ${t.border}` }}>
        {SESSIONS.map(session => <SessionItem key={session.id} session={session} t={t} />)}
      </div>

      <div style={{
        height: 48,
        borderBottom: `1px solid ${t.border}`,
        display: 'flex',
        alignItems: 'center',
        gap: 10,
        padding: '0 12px',
        color: t.textSecondary,
        flexShrink: 0,
      }}>
        <span style={{ color: t.muted, fontSize: 13 }}>⌃</span>
        <span style={{ fontSize: 16 }}>▤</span>
        <span style={{ fontSize: 16, flex: 1, minWidth: 0, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>
          ~/Desktop/P...
        </span>
        <span style={{ color: t.muted, fontSize: 16 }}>◉</span>
        <span style={{ color: t.muted, fontSize: 18 }}>↻</span>
      </div>

      <div style={{ flex: 1, minHeight: 0, overflowY: 'auto', padding: '6px 0' }}>
        {FOLDERS.map(folder => <FolderRow key={folder} label={folder} t={t} />)}
      </div>
    </aside>
  )
}

function SectionHeader({ label, action, t }: { label: string; action?: ReactNode; t: EguiTokens }) {
  return (
    <div style={{
      height: 40,
      display: 'flex',
      alignItems: 'center',
      justifyContent: 'space-between',
      padding: '0 10px',
      borderBottom: `1px solid ${t.border}`,
      boxSizing: 'border-box',
      flexShrink: 0,
    }}>
      <span style={{
        fontSize: 12,
        color: t.muted,
        letterSpacing: '0.06em',
      }}>
        {label}
      </span>
      {action}
    </div>
  )
}

function IconButton({ label, t }: { label: string; t: EguiTokens }) {
  return (
    <button style={{
      width: 26,
      height: 24,
      display: 'flex',
      alignItems: 'center',
      justifyContent: 'center',
      backgroundColor: t.input,
      border: `1px solid ${t.border}`,
      color: t.muted,
      fontFamily: 'inherit',
      fontSize: 15,
      cursor: 'pointer',
    }}>
      {label}
    </button>
  )
}

function SessionItem({ session, t }: { session: Session; t: EguiTokens }) {
  const active = session.selected
  return (
    <div style={{
      display: 'grid',
      gridTemplateColumns: '5px minmax(0, 1fr)',
      minHeight: 84,
      backgroundColor: active ? t.navActive : 'transparent',
      border: `1px solid ${active ? t.navActive : 'transparent'}`,
      marginBottom: 6,
      overflow: 'hidden',
      cursor: 'default',
    }}>
      <div style={{ backgroundColor: session.accent }} />
      <div style={{ padding: '9px 14px 8px 18px', minWidth: 0 }}>
        <div style={{
          display: 'flex',
          alignItems: 'center',
          gap: 8,
          minWidth: 0,
        }}>
          <span style={{
            fontSize: 21,
            lineHeight: '24px',
            color: active ? t.navActiveText : t.textSecondary,
            overflow: 'hidden',
            textOverflow: 'ellipsis',
            whiteSpace: 'nowrap',
            flex: 1,
          }}>
            {session.name}
          </span>
          <StatusBadge status={session.status} t={t} />
        </div>
        <div style={{
          marginTop: 4,
          fontSize: 14,
          fontWeight: 600,
          color: t.textSecondary,
          letterSpacing: '0.02em',
          overflow: 'hidden',
          textOverflow: 'ellipsis',
          whiteSpace: 'nowrap',
        }}>
          {session.model}
        </div>
        <div style={{ marginTop: 4, fontSize: 14, color: t.muted }}>
          {session.status === 'running' ? '실행 중' : '유휴'}
        </div>
      </div>
    </div>
  )
}

function StatusBadge({ status, t }: { status: SessionStatus; t: EguiTokens }) {
  const running = status === 'running'
  return (
    <span style={{
      fontSize: 9,
      padding: '1px 5px',
      backgroundColor: running ? t.tag : t.input,
      color: running ? t.tagText : t.muted,
      border: `1px solid ${running ? t.tag : t.border}`,
      letterSpacing: '0.04em',
      flexShrink: 0,
    }}>
      {running ? 'RUN' : 'IDLE'}
    </span>
  )
}

function FolderRow({ label, t }: { label: string; t: EguiTokens }) {
  return (
    <div style={{
      height: 34,
      display: 'flex',
      alignItems: 'center',
      gap: 10,
      padding: '0 14px',
      color: t.textSecondary,
      fontSize: 18,
      boxSizing: 'border-box',
    }}>
      <span style={{ color: t.muted, fontSize: 12 }}>▸</span>
      <span style={{ color: t.muted, fontSize: 16 }}>▤</span>
      <span style={{ overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>{label}</span>
    </div>
  )
}

function TerminalArea({ t }: { t: EguiTokens }) {
  return (
    <section style={{
      minWidth: 0,
      flex: 1,
      display: 'flex',
      flexDirection: 'column',
      backgroundColor: t.input,
    }}>
      <HeaderBar
        title="◆  Design  ×"
        t={t}
        right={
          <div style={{ display: 'flex', gap: 14, color: t.muted, fontSize: 18 }}>
            <span>＋</span>
            <span>▣</span>
            <span>▤</span>
          </div>
        }
      />
      <div style={{ flex: 1, minHeight: 0, overflowY: 'auto', padding: '18px 28px', boxSizing: 'border-box' }}>
        <TerminalContent t={t} />
      </div>
    </section>
  )
}

function TerminalContent({ t }: { t: EguiTokens }) {
  return (
    <div style={{ display: 'flex', flexDirection: 'column', gap: 18 }}>
      <TerminalText tone="default" t={t}>jr@jrui-MacBookAir ~ % cd '/Users' && codex resume 019f3d2f-ce5c-7422-b103-60e990f3bdf6</TerminalText>
      <TerminalBox t={t}>
        {'>_ OpenAI Codex (v0.142.5)\n\nmodel:     loading   /model to change\ndirectory: /Users'}
      </TerminalBox>
      <TerminalText tone="warn" t={t}>Ignored invalid status line item: "thread".</TerminalText>
      <TerminalBox t={t}>
        {'>_ OpenAI Codex (v0.142.5)\n\nmodel:     gpt-5.5 high   /model to change\ndirectory: /Users'}
      </TerminalBox>
      <TerminalText tone="default" t={t}>Tip: Try the Codex App. Run 'codex app' or visit https://chatgpt.com/codex?app-landing-page=true</TerminalText>
      <div style={{
        backgroundColor: t.surfaceHover,
        color: t.text,
        padding: '14px 22px',
        fontSize: 19,
      }}>
        › hi
      </div>
      <TerminalText tone="error" t={t}>■ Conversation interrupted - tell the model what to do differently.</TerminalText>
      <TerminalText tone="warn" t={t}>`--dangerously-bypass-hook-trust` is enabled. Enabled hooks may run without review for this invocation.</TerminalText>
      <TerminalText tone="default" t={t}>• You have 4 usage limit resets available. Run /usage to use one.</TerminalText>
      <TerminalText tone="warn" t={t}>MCP startup incomplete (failed: ahto)</TerminalText>
      <TerminalText tone="accent" t={t}>› [Image #1] [Image #2] [Image #3] [Image #4] ▌</TerminalText>
      <TerminalText tone="status" t={t}>gpt-5.5 high · /Users</TerminalText>
    </div>
  )
}

function TerminalBox({ t, children }: { t: EguiTokens; children: string }) {
  return (
    <pre style={{
      width: 'fit-content',
      maxWidth: '100%',
      margin: 0,
      whiteSpace: 'pre-wrap',
      border: `1px solid ${t.textSecondary}`,
      backgroundColor: t.input,
      color: t.text,
      padding: '12px 18px',
      fontFamily: 'inherit',
      fontSize: 18,
      lineHeight: 1.45,
    }}>
      {children}
    </pre>
  )
}

function TerminalText({ tone, t, children }: {
  tone: 'default' | 'warn' | 'error' | 'accent' | 'status'
  t: EguiTokens
  children: ReactNode
}) {
  const color = {
    default: t.textSecondary,
    warn: '#c8a12d',
    error: t.danger,
    accent: t.accent,
    status: '#d4c889',
  }[tone]

  return (
    <div style={{
      color,
      fontSize: 18,
      lineHeight: 1.45,
      overflow: 'hidden',
      textOverflow: 'ellipsis',
      whiteSpace: 'nowrap',
    }}>
      {tone === 'warn' && <span style={{ marginRight: 6 }}>⚠</span>}
      {children}
    </div>
  )
}

function RenamePage({ t }: { t: EguiTokens }) {
  return (
    <div style={{ width: 440 }}>
      <SectionHeader label="세션 이름 편집" t={t} />
      <div style={{ padding: 10, border: `1px solid ${t.border}`, borderTop: 'none', backgroundColor: t.panel }}>
        <input
          value="경매작가"
          readOnly
          autoFocus
          style={{
            width: '100%',
            height: 39,
            boxSizing: 'border-box',
            backgroundColor: t.input,
            border: `1px solid ${t.borderFocus}`,
            outline: `1px solid ${t.borderFocus}`,
            outlineOffset: -1,
            color: t.text,
            fontFamily: 'inherit',
            fontSize: 24,
            padding: '3px 8px',
          }}
        />
        <div style={{ marginTop: 8 }}>
          <SessionItem
            t={t}
            session={{ id: 'tennis', name: 'tennisssss', model: 'Claude · claude-opus-4-8', status: 'running', accent: t.accent }}
          />
        </div>
      </div>
    </div>
  )
}

function ClosePanePage({ t }: { t: EguiTokens }) {
  return (
    <PopupDialog
      title="pane 닫기"
      message="실행 중인 세션이 종료됩니다. 닫을까요?"
      t={t}
      actions={[
        { label: '닫기', variant: 'danger' },
        { label: '취소' },
      ]}
    />
  )
}

function PopupDialog({
  title,
  message,
  actions,
  t,
}: {
  title: string
  message: string
  actions: { label: string; variant?: 'default' | 'danger' }[]
  t: EguiTokens
}) {
  return (
    <div style={{
      width: POPUP_RULES.width,
      border: `1px solid ${t.border}`,
      backgroundColor: t.input,
      boxShadow: '0 8px 24px rgba(0,0,0,0.35)',
    }}>
      <div style={{
        height: POPUP_RULES.titleHeight,
        display: 'flex',
        alignItems: 'center',
        justifyContent: 'center',
        position: 'relative',
        backgroundColor: t.surface,
        borderBottom: `1px solid ${t.border}`,
      }}>
        <span style={{
          fontSize: POPUP_RULES.titleFontSize,
          color: t.text,
          textAlign: 'center',
          lineHeight: 1,
        }}>
          {title}
        </span>
        <span style={{
          position: 'absolute',
          right: 28,
          top: '50%',
          transform: 'translateY(-50%)',
          fontSize: 44,
          color: t.muted,
          lineHeight: 1,
        }}>
          ×
        </span>
      </div>
      <div style={{
        minHeight: POPUP_RULES.bodyMinHeight,
        padding: POPUP_RULES.bodyPadding,
        boxSizing: 'border-box',
        display: 'flex',
        flexDirection: 'column',
        alignItems: 'center',
        justifyContent: 'center',
      }}>
        <div style={{
          width: '100%',
          color: t.text,
          fontSize: POPUP_RULES.messageFontSize,
          fontWeight: 600,
          lineHeight: 1.35,
          textAlign: 'center',
          wordBreak: 'keep-all',
        }}>
          {message}
        </div>
        <div style={{
          display: 'flex',
          justifyContent: 'center',
          gap: POPUP_RULES.buttonGap,
          marginTop: 28,
        }}>
          {actions.map(action => (
            <CommandButton
              key={action.label}
              label={action.label}
              t={t}
              variant={action.variant}
            />
          ))}
        </div>
      </div>
    </div>
  )
}

function CommandButton({ label, t, variant = 'default' }: {
  label: string
  t: EguiTokens
  variant?: 'default' | 'danger'
}) {
  const danger = variant === 'danger'
  return (
    <button style={{
      minWidth: POPUP_RULES.buttonMinWidth,
      height: POPUP_RULES.buttonHeight,
      display: 'flex',
      alignItems: 'center',
      justifyContent: 'center',
      padding: '0 20px',
      fontSize: 24,
      fontWeight: 600,
      fontFamily: 'inherit',
      backgroundColor: danger ? t.danger : t.surface,
      border: `1px solid ${danger ? t.danger : t.border}`,
      color: danger ? t.dangerText : t.text,
      cursor: 'pointer',
      textAlign: 'center',
      lineHeight: 1,
    }}>
      {label}
    </button>
  )
}

function ContextMenuPage({ t }: { t: EguiTokens }) {
  return (
    <div style={{
      width: 260,
      backgroundColor: t.panel,
      border: `1px solid ${t.border}`,
      boxShadow: '0 8px 24px rgba(0,0,0,0.35)',
      padding: 6,
    }}>
      <MenuItem label="새 폴더 (이 안에)" selected t={t} />
      <MenuItem label="이름 변경" t={t} />
      <MenuItem label="휴지통으로 삭제" t={t} />
      <div style={{ borderTop: `1px solid ${t.border}`, margin: '6px 6px' }} />
      <MenuItem label="경로 복사" t={t} />
      <MenuItem label="터미널에 경로 삽입" t={t} />
    </div>
  )
}

function MenuItem({ label, selected, t }: { label: string; selected?: boolean; t: EguiTokens }) {
  return (
    <div style={{
      height: 34,
      display: 'flex',
      alignItems: 'center',
      padding: '0 12px',
      backgroundColor: selected ? t.surfaceHover : 'transparent',
      color: selected ? t.text : t.textSecondary,
      fontSize: 20,
      boxSizing: 'border-box',
    }}>
      {label}
    </div>
  )
}
