import { useState } from 'react'
import { egui, type Theme } from './egui'

type NavItem = string

interface NavGroup {
  label: string
  items: { icon: string; label: NavItem; badge?: number }[]
}

const NAV_GROUPS: NavGroup[] = [
  {
    label: '설정',
    items: [
      { icon: '◎', label: '일반' },
      { icon: '⊕', label: '언어' },
      { icon: '▣', label: '터미널' },
      { icon: '⚡', label: '성능' },
      { icon: '⚿', label: '원격 서버 (TLS)' },
    ],
  },
  {
    label: '관리',
    items: [
      { icon: '⚷', label: '자격증명' },
      { icon: '⚯', label: '연결 (커넥터)' },
      { icon: '▤', label: '환경 프로필' },
      { icon: '◈', label: '에이전트' },
      { icon: '▢', label: '워크스페이스' },
    ],
  },
  {
    label: '모니터',
    items: [
      { icon: '◷', label: '활동' },
      { icon: '◍', label: '알림', badge: 2 },
    ],
  },
]

interface Props {
  theme: Theme
  activeNav: NavItem
  onSelect: (v: NavItem) => void
  search: string
  onSearch: (v: string) => void
}

export default function Sidebar({ theme, activeNav, onSelect, search, onSearch }: Props) {
  const t = egui(theme)
  const [hoveredItem, setHoveredItem] = useState<string | null>(null)

  return (
    <div style={{
      width: 190,
      borderRight: `1px solid ${t.border}`,
      display: 'flex',
      flexDirection: 'column',
      backgroundColor: t.panel,
      flexShrink: 0,
    }}>
      {/* Search */}
      <div style={{ padding: '8px 8px 6px', borderBottom: `1px solid ${t.border}` }}>
        <div style={{ position: 'relative' }}>
          <span style={{
            position: 'absolute',
            left: 7,
            top: '50%',
            transform: 'translateY(-50%)',
            fontSize: 10,
            color: t.muted,
            pointerEvents: 'none',
          }}>⌕</span>
          <input
            value={search}
            onChange={e => onSearch(e.target.value)}
            placeholder="검색"
            style={{
              width: '100%',
              padding: '4px 8px 4px 22px',
              fontSize: 11,
              fontFamily: 'inherit',
              backgroundColor: t.input,
              border: `1px solid ${t.inputBorder}`,
              color: t.text,
              outline: 'none',
              boxSizing: 'border-box',
            }}
          />
        </div>
      </div>

      {/* Nav groups */}
      <div style={{ flex: 1, overflowY: 'auto', padding: '4px 0' }}>
        {NAV_GROUPS.map(group => (
          <div key={group.label}>
            <div style={{
              padding: '8px 12px 3px',
              fontSize: 10,
              color: t.muted,
              letterSpacing: '0.06em',
              textTransform: 'uppercase',
            }}>
              {group.label}
            </div>
            {group.items.map(item => {
              const isActive = item.label === activeNav
              const isHover = hoveredItem === item.label && !isActive
              return (
                <div
                  key={item.label}
                  onClick={() => onSelect(item.label)}
                  onMouseEnter={() => setHoveredItem(item.label)}
                  onMouseLeave={() => setHoveredItem(null)}
                  style={{
                    display: 'flex',
                    alignItems: 'center',
                    gap: 8,
                    padding: '5px 12px',
                    cursor: 'pointer',
                    backgroundColor: isActive ? t.navActive : isHover ? t.surfaceHover : 'transparent',
                    color: isActive ? t.navActiveText : t.textSecondary,
                    fontSize: 12,
                    userSelect: 'none',
                    transition: 'background-color 0.08s',
                  }}
                >
                  <span style={{ fontSize: 11, width: 14, textAlign: 'center', color: t.muted }}>{item.icon}</span>
                  <span style={{ flex: 1 }}>{item.label}</span>
                  {item.badge && (
                    <span style={{
                      backgroundColor: t.accent,
                      color: t.accentText,
                      fontSize: 10,
                      padding: '1px 5px',
                      minWidth: 16,
                      textAlign: 'center',
                    }}>
                      {item.badge}
                    </span>
                  )}
                </div>
              )
            })}
          </div>
        ))}
      </div>
    </div>
  )
}
