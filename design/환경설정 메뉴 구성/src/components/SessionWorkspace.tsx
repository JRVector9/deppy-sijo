import { useState, type ReactNode } from 'react'
import { egui, type EguiTokens, type Theme } from './egui'
import ComponentGuide from './ComponentGuide'

type Page = 'components' | 'workspace' | 'rename' | 'close' | 'menu'
type SessionStatus = 'running' | 'idle'

interface Session {
  id: string
  name: string
  model: string
  status: SessionStatus
  accent: string
  selected?: boolean
}

const PAGE_LABELS: { id: Page; label: string }[] = [
  { id: 'components', label: '컴포넌트 요소' },
  { id: 'workspace', label: '전체 화면' },
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

export default function SessionWorkspace() {
  const [theme, setTheme] = useState<Theme>('dark')
  const [page, setPage] = useState<Page>('components')
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
        {page === 'workspace' && <WorkspacePage t={t} />}
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
    <WindowFrame t={t} width={1280} height={760}>
      <div style={{ display: 'flex', height: '100%', minWidth: 0 }}>
        <SessionSidebar t={t} />
        <TerminalArea t={t} />
      </div>
    </WindowFrame>
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
