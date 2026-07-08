import { useState } from 'react'
import SettingsPanel from './components/SettingsPanel'
import ComponentGuide from './components/ComponentGuide'
import { egui, type Theme } from './components/egui'

type View = 'settings' | 'guide'

export default function App() {
  const [theme, setTheme] = useState<Theme>('dark')
  const [view, setView] = useState<View>('settings')
  const t = egui(theme)

  return (
    <div style={{
      width: '100%',
      minHeight: '100vh',
      backgroundColor: theme === 'dark' ? '#1a1a1a' : '#e0e0e0',
      fontFamily: '"JetBrains Mono", "Fira Mono", "Consolas", monospace',
    }}>
      {/* Top nav bar */}
      <div style={{
        display: 'flex',
        alignItems: 'center',
        gap: 1,
        padding: '8px 16px',
        borderBottom: `1px solid ${t.border}`,
        backgroundColor: t.panel,
      }}>
        <div style={{ display: 'flex', border: `1px solid ${t.border}`, marginRight: 12 }}>
          {([
            { id: 'settings', label: '설정 패널' },
            { id: 'guide', label: '컴포넌트 가이드' },
          ] as { id: View; label: string }[]).map(v => (
            <button
              key={v.id}
              onClick={() => setView(v.id)}
              style={{
                padding: '4px 14px',
                fontSize: 11,
                fontFamily: 'inherit',
                cursor: 'pointer',
                border: 'none',
                borderRight: v.id === 'settings' ? `1px solid ${t.border}` : 'none',
                backgroundColor: view === v.id ? t.accent : t.input,
                color: view === v.id ? t.accentText : t.muted,
                letterSpacing: '0.02em',
                transition: 'background-color 0.08s, color 0.08s',
              }}
            >{v.label}</button>
          ))}
        </div>
        <span style={{ fontSize: 10, color: t.muted, marginLeft: 'auto' }}>egui constraint system · mono grid</span>
      </div>

      {/* View */}
      {view === 'settings' && (
        <div style={{
          display: 'flex',
          alignItems: 'center',
          justifyContent: 'center',
          minHeight: 'calc(100vh - 42px)',
          padding: 24,
        }}>
          <SettingsPanel theme={theme} onThemeChange={setTheme} />
        </div>
      )}
      {view === 'guide' && <ComponentGuide />}
    </div>
  )
}
