import { useState } from 'react'
import { egui, type Theme } from './egui'
import Sidebar from './Sidebar'
import EnvProfilePanel from './EnvProfilePanel'

type NavItem =
  | '일반' | '언어' | '터미널' | '성능' | '원격 서버 (TLS)'
  | '자격증명' | '연결 (커넥터)' | '환경 설정' | '에이전트' | '워크스페이스'
  | '활동' | '알림'

interface Props {
  theme: Theme
  onThemeChange: (t: Theme) => void
}

export default function SettingsPanel({ theme, onThemeChange }: Props) {
  const t = egui(theme)
  const [activeNav, setActiveNav] = useState<NavItem>('환경 설정')
  const [search, setSearch] = useState('')

  return (
    <div style={{
      width: 860,
      height: 560,
      backgroundColor: t.surface,
      border: `1px solid ${t.border}`,
      display: 'flex',
      overflow: 'hidden',
      boxShadow: theme === 'dark'
        ? '0 8px 32px rgba(0,0,0,0.6)'
        : '0 8px 32px rgba(0,0,0,0.18)',
    }}>
      {/* Sidebar */}
      <Sidebar
        theme={theme}
        activeNav={activeNav}
        onSelect={(v) => setActiveNav(v as NavItem)}
        search={search}
        onSearch={setSearch}
      />

      {/* Main content */}
      <div style={{ flex: 1, display: 'flex', flexDirection: 'column', overflow: 'hidden' }}>
        {/* Header bar */}
        <div style={{
          height: 36,
          borderBottom: `1px solid ${t.border}`,
          display: 'flex',
          alignItems: 'center',
          justifyContent: 'space-between',
          padding: '0 16px',
          flexShrink: 0,
        }}>
          <span style={{ fontSize: 13, fontWeight: 600, color: t.text, letterSpacing: '0.02em' }}>
            {activeNav}
          </span>
          <ThemeToggle theme={theme} onChange={onThemeChange} t={t} />
        </div>

        {/* Content area */}
        <div style={{ flex: 1, overflow: 'hidden' }}>
          {activeNav === '환경 설정' && <EnvProfilePanel theme={theme} />}
          {activeNav !== '환경 설정' && (
            <div style={{ padding: 24, color: t.muted, fontSize: 12 }}>
              {activeNav} 설정 패널
            </div>
          )}
        </div>
      </div>
    </div>
  )
}

function ThemeToggle({ theme, onChange, t }: { theme: Theme; onChange: (v: Theme) => void; t: ReturnType<typeof egui> }) {
  const options: { label: string; value: Theme }[] = [
    { label: '시스템', value: 'dark' },
    { label: '라이트', value: 'light' },
    { label: '다크', value: 'dark' },
  ]
  const themeOptions = [
    { label: '라이트', value: 'light' as Theme },
    { label: '다크', value: 'dark' as Theme },
  ]

  return (
    <div style={{ display: 'flex', gap: 1, border: `1px solid ${t.border}` }}>
      {themeOptions.map(opt => (
        <button
          key={opt.value}
          onClick={() => onChange(opt.value)}
          style={{
            padding: '3px 10px',
            fontSize: 11,
            fontFamily: 'inherit',
            cursor: 'pointer',
            border: 'none',
            backgroundColor: theme === opt.value ? t.accent : t.input,
            color: theme === opt.value ? t.accentText : t.muted,
            transition: 'background-color 0.1s',
          }}
        >
          {opt.label}
        </button>
      ))}
    </div>
  )
}
