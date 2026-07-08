import { useState } from 'react'
import { egui, type Theme, type EguiTokens } from './egui'

// ─── small primitives ────────────────────────────────────────────────────────

function Label({ children, t }: { children: React.ReactNode; t: EguiTokens }) {
  return (
    <span style={{
      fontSize: 9,
      color: t.muted,
      letterSpacing: '0.07em',
      textTransform: 'uppercase',
      display: 'block',
      marginBottom: 6,
    }}>
      {children}
    </span>
  )
}

function Annotation({ children, t }: { children: React.ReactNode; t: EguiTokens }) {
  return (
    <span style={{
      fontSize: 9,
      color: t.muted,
      letterSpacing: '0.04em',
      marginTop: 4,
      display: 'block',
    }}>
      {children}
    </span>
  )
}

function SectionTitle({ children, t }: { children: React.ReactNode; t: EguiTokens }) {
  return (
    <div style={{
      fontSize: 10,
      fontWeight: 700,
      color: t.text,
      letterSpacing: '0.1em',
      textTransform: 'uppercase',
      padding: '14px 0 8px',
      borderBottom: `1px solid ${t.border}`,
      marginBottom: 14,
    }}>
      {children}
    </div>
  )
}

function Row({ children, gap = 16 }: { children: React.ReactNode; gap?: number }) {
  return (
    <div style={{ display: 'flex', gap, alignItems: 'flex-start', flexWrap: 'wrap', marginBottom: 16 }}>
      {children}
    </div>
  )
}

function Cell({ children, label, t }: { children: React.ReactNode; label?: string; t: EguiTokens }) {
  return (
    <div style={{ display: 'flex', flexDirection: 'column' }}>
      {label && <Label t={t}>{label}</Label>}
      {children}
    </div>
  )
}

// ─── color swatch ─────────────────────────────────────────────────────────────

function Swatch({ color, name, t }: { color: string; name: string; t: EguiTokens }) {
  return (
    <div style={{ width: 56 }}>
      <div style={{
        width: 56,
        height: 28,
        backgroundColor: color,
        border: `1px solid ${t.border}`,
      }} />
      <div style={{ fontSize: 8, color: t.muted, marginTop: 3, lineHeight: 1.4 }}>
        <div style={{ color: t.textSecondary }}>{name}</div>
        <div>{color}</div>
      </div>
    </div>
  )
}

// ─── button variants ──────────────────────────────────────────────────────────

function BtnDemo({ label, bg, border, color, t, annotation }: {
  label: string; bg: string; border: string; color: string; t: EguiTokens; annotation?: string
}) {
  return (
    <Cell t={t} label={annotation}>
      <button style={{
        padding: '4px 12px',
        fontSize: 11,
        fontFamily: 'inherit',
        backgroundColor: bg,
        border: `1px solid ${border}`,
        color,
        cursor: 'default',
        letterSpacing: '0.02em',
      }}>
        {label}
      </button>
    </Cell>
  )
}

// ─── input variants ───────────────────────────────────────────────────────────

function InputDemo({ placeholder, value, focused, t, label }: {
  placeholder?: string; value?: string; focused?: boolean; t: EguiTokens; label?: string
}) {
  return (
    <Cell t={t} label={label}>
      <div style={{
        padding: '4px 8px',
        fontSize: 11,
        fontFamily: 'inherit',
        backgroundColor: t.input,
        border: `1px solid ${focused ? t.borderFocus : t.inputBorder}`,
        color: value ? t.text : t.muted,
        width: 160,
        outline: focused ? `1px solid ${t.borderFocus}` : 'none',
        outlineOffset: -1,
      }}>
        {value || placeholder || ''}
      </div>
    </Cell>
  )
}

// ─── nav item ─────────────────────────────────────────────────────────────────

function NavItemDemo({ label, state, icon, badge, t }: {
  label: string; state: 'default' | 'hover' | 'active'; icon?: string; badge?: number; t: EguiTokens
}) {
  const bg = state === 'active' ? t.navActive : state === 'hover' ? t.surfaceHover : 'transparent'
  const color = state === 'active' ? t.navActiveText : t.textSecondary
  return (
    <div style={{
      display: 'flex',
      alignItems: 'center',
      gap: 8,
      padding: '5px 10px',
      backgroundColor: bg,
      color,
      fontSize: 12,
      userSelect: 'none',
      width: 160,
    }}>
      <span style={{ fontSize: 11, width: 14, textAlign: 'center', color: t.muted }}>{icon || '▤'}</span>
      <span style={{ flex: 1 }}>{label}</span>
      {badge != null && (
        <span style={{
          backgroundColor: t.accent,
          color: t.accentText,
          fontSize: 9,
          padding: '1px 5px',
        }}>{badge}</span>
      )}
    </div>
  )
}

// ─── project list item ────────────────────────────────────────────────────────

function ProjectItemDemo({ name, path, envCount, keyCount, state, t }: {
  name: string; path: string; envCount: number; keyCount: number;
  state: 'default' | 'hover' | 'active'; t: EguiTokens
}) {
  const isActive = state === 'active'
  const isHover = state === 'hover'
  return (
    <div style={{
      display: 'flex',
      alignItems: 'center',
      gap: 6,
      padding: '6px 10px',
      backgroundColor: isActive ? t.navActive : isHover ? t.surfaceHover : 'transparent',
      border: `1px solid ${t.border}`,
      width: 186,
      position: 'relative',
      boxSizing: 'border-box',
    }}>
      <span style={{ fontSize: 11, color: isActive ? t.accent : t.muted }}>{isActive ? '▤' : '▤'}</span>
      <div style={{ flex: 1, minWidth: 0 }}>
        <div style={{
          fontSize: 11,
          fontWeight: isActive ? 600 : 400,
          color: isActive ? t.text : t.textSecondary,
          overflow: 'hidden',
          textOverflow: 'ellipsis',
          whiteSpace: 'nowrap',
        }}>{name}</div>
        <div style={{
          fontSize: 9,
          color: t.muted,
          overflow: 'hidden',
          textOverflow: 'ellipsis',
          whiteSpace: 'nowrap',
          marginTop: 1,
        }}>{path}</div>
      </div>
      <div style={{ display: 'flex', flexDirection: 'column', alignItems: 'flex-end', gap: 2 }}>
        <span style={{ fontSize: 9, color: t.muted }}>{envCount}env</span>
        <span style={{ fontSize: 9, color: t.muted }}>{keyCount}key</span>
      </div>
      {isHover && (
        <div style={{
          position: 'absolute', right: 4, top: 4,
          width: 14, height: 14,
          border: `1px solid ${t.border}`,
          color: t.muted,
          fontSize: 9,
          display: 'flex', alignItems: 'center', justifyContent: 'center',
        }}>×</div>
      )}
    </div>
  )
}

// ─── env var row ──────────────────────────────────────────────────────────────

function EnvRowDemo({ keyName, value, masked, revealed, hover, t, label }: {
  keyName: string; value: string; masked?: boolean; revealed?: boolean; hover?: boolean; t: EguiTokens; label?: string
}) {
  const displayVal = masked && !revealed ? '••••••••••••' : value
  return (
    <Cell t={t} label={label}>
      <div style={{
        display: 'grid',
        gridTemplateColumns: '120px 120px 22px 22px',
        gap: 4,
        alignItems: 'center',
        padding: '3px 4px',
        backgroundColor: hover ? t.surfaceHover : 'transparent',
        border: `1px solid ${t.border}`,
        width: 'fit-content',
      }}>
        <span style={{ fontSize: 11, color: t.accent, letterSpacing: '0.02em', padding: '0 2px' }}>{keyName}</span>
        <span style={{ fontSize: 11, color: masked && !revealed ? t.muted : t.text, padding: '0 2px' }}>{displayVal}</span>
        <span style={{
          width: 22, height: 18, fontSize: 10,
          display: 'flex', alignItems: 'center', justifyContent: 'center',
          border: `1px solid ${hover ? t.border : 'transparent'}`,
          color: masked ? t.accent : t.muted,
        }}>{masked ? (revealed ? '○' : '●') : '○'}</span>
        <span style={{
          width: 22, height: 18, fontSize: 11,
          display: 'flex', alignItems: 'center', justifyContent: 'center',
          border: `1px solid ${hover ? t.border : 'transparent'}`,
          color: t.muted,
        }}>×</span>
      </div>
    </Cell>
  )
}

// ─── api key row ──────────────────────────────────────────────────────────────

function ApiRowDemo({ provider, label: rowLabel, type, secret, hover, t, label }: {
  provider: string; label: string; type: 'api_key' | 'token'; secret: string;
  hover?: boolean; t: EguiTokens; label?: string
}) {
  return (
    <Cell t={t} label={label}>
      <div style={{
        display: 'grid',
        gridTemplateColumns: '72px 80px 52px 100px 22px',
        gap: 4,
        alignItems: 'center',
        padding: '3px 4px',
        backgroundColor: hover ? t.surfaceHover : 'transparent',
        border: `1px solid ${t.border}`,
        width: 'fit-content',
      }}>
        <span style={{ fontSize: 11, color: t.textSecondary, padding: '0 2px' }}>{provider}</span>
        <span style={{ fontSize: 11, color: t.text, padding: '0 2px' }}>{rowLabel}</span>
        <span style={{
          fontSize: 9, padding: '2px 5px',
          backgroundColor: type === 'token' ? t.accent : t.input,
          border: `1px solid ${type === 'token' ? t.accent : t.border}`,
          color: type === 'token' ? t.accentText : t.muted,
          textAlign: 'center',
        }}>{type}</span>
        <span style={{ fontSize: 11, color: t.muted, padding: '0 2px' }}>{secret}</span>
        <span style={{
          width: 22, height: 18, fontSize: 11,
          display: 'flex', alignItems: 'center', justifyContent: 'center',
          border: `1px solid ${hover ? t.border : 'transparent'}`,
          color: t.muted,
        }}>×</span>
      </div>
    </Cell>
  )
}

// ─── type toggle ──────────────────────────────────────────────────────────────

function TypeToggleDemo({ active, t, label }: { active: 'api_key' | 'token'; t: EguiTokens; label?: string }) {
  return (
    <Cell t={t} label={label}>
      <div style={{ display: 'flex', gap: 6, alignItems: 'center' }}>
        {(['api_key', 'token'] as const).map(v => (
          <span key={v} style={{
            fontSize: 10,
            padding: '3px 8px',
            backgroundColor: active === v ? t.accent : t.input,
            border: `1px solid ${active === v ? t.accent : t.border}`,
            color: active === v ? t.accentText : t.muted,
            letterSpacing: '0.02em',
            cursor: 'default',
          }}>{v}</span>
        ))}
      </div>
    </Cell>
  )
}

// ─── badge / tag ──────────────────────────────────────────────────────────────

function BadgeDemo({ value, t, label }: { value: string | number; t: EguiTokens; label?: string }) {
  return (
    <Cell t={t} label={label}>
      <span style={{
        fontSize: 9,
        padding: '1px 5px',
        backgroundColor: t.tag,
        color: t.tagText,
      }}>{value}</span>
    </Cell>
  )
}

function NotifBadge({ value, t, label }: { value: number; t: EguiTokens; label?: string }) {
  return (
    <Cell t={t} label={label}>
      <span style={{
        fontSize: 9,
        padding: '1px 6px',
        backgroundColor: t.accent,
        color: t.accentText,
      }}>{value}</span>
    </Cell>
  )
}

// ─── section header ───────────────────────────────────────────────────────────

function SectionHeaderDemo({ label: secLabel, count, t, label }: {
  label: string; count: number; t: EguiTokens; label?: string
}) {
  return (
    <Cell t={t} label={label}>
      <div style={{
        display: 'flex',
        alignItems: 'center',
        justifyContent: 'space-between',
        padding: '6px 0 4px',
        borderBottom: `1px solid ${t.border}`,
        width: 260,
      }}>
        <div style={{ display: 'flex', alignItems: 'center', gap: 6 }}>
          <span style={{ fontSize: 11, fontWeight: 600, color: t.text }}>{secLabel}</span>
          <span style={{ fontSize: 9, padding: '1px 5px', backgroundColor: t.tag, color: t.tagText }}>{count}</span>
        </div>
        <span style={{
          fontSize: 10, padding: '2px 8px',
          backgroundColor: t.input, border: `1px solid ${t.border}`,
          color: t.muted, cursor: 'default',
        }}>+ 추가</span>
      </div>
    </Cell>
  )
}

// ─── stepper control ─────────────────────────────────────────────────────────

function StepperDemo({ value, t, label }: { value: number; t: EguiTokens; label?: string }) {
  return (
    <Cell t={t} label={label}>
      <div style={{ display: 'flex', border: `1px solid ${t.inputBorder}` }}>
        <span style={{
          width: 40, textAlign: 'center', fontSize: 12,
          backgroundColor: t.input, color: t.text, padding: '3px 0',
          borderRight: `1px solid ${t.inputBorder}`,
          fontVariantNumeric: 'tabular-nums',
        }}>{value}</span>
        <span style={{
          width: 22, display: 'flex', alignItems: 'center', justifyContent: 'center',
          fontSize: 11, color: t.muted, borderRight: `1px solid ${t.inputBorder}`,
          backgroundColor: t.input,
        }}>−</span>
        <span style={{
          width: 22, display: 'flex', alignItems: 'center', justifyContent: 'center',
          fontSize: 11, color: t.muted, backgroundColor: t.input,
        }}>+</span>
      </div>
    </Cell>
  )
}

// ─── theme toggle strip ───────────────────────────────────────────────────────

function ThemeToggleDemo({ active, t, label }: { active: 'light' | 'dark'; t: EguiTokens; label?: string }) {
  return (
    <Cell t={t} label={label}>
      <div style={{ display: 'flex', border: `1px solid ${t.border}`, gap: 1 }}>
        {(['라이트', '다크'] as const).map(v => {
          const val = v === '라이트' ? 'light' : 'dark'
          const sel = active === val
          return (
            <span key={v} style={{
              padding: '3px 10px', fontSize: 11, cursor: 'default',
              backgroundColor: sel ? t.accent : t.input,
              color: sel ? t.accentText : t.muted,
            }}>{v}</span>
          )
        })}
      </div>
    </Cell>
  )
}

// ─── scrollbar indicator ──────────────────────────────────────────────────────

function ScrollbarDemo({ t, label }: { t: EguiTokens; label?: string }) {
  return (
    <Cell t={t} label={label}>
      <div style={{ display: 'flex', gap: 6, alignItems: 'center' }}>
        <div style={{ width: 3, height: 48, backgroundColor: t.surface, border: `1px solid ${t.border}`, position: 'relative' }}>
          <div style={{
            position: 'absolute', top: 8, left: 0, right: 0,
            height: 16, backgroundColor: t.scrollbar,
          }} />
        </div>
        <Annotation t={t}>3px wide · solid · no radius</Annotation>
      </div>
    </Cell>
  )
}

// ─── layout diagram ───────────────────────────────────────────────────────────

function LayoutDiagram({ t }: { t: EguiTokens }) {
  const box = (label: string, w: number | string, h: number, bg: string, color?: string) => (
    <div style={{
      width: w,
      height: h,
      backgroundColor: bg,
      border: `1px solid ${t.border}`,
      display: 'flex',
      alignItems: 'center',
      justifyContent: 'center',
      fontSize: 8,
      color: color || t.muted,
      letterSpacing: '0.04em',
      boxSizing: 'border-box',
      flexShrink: 0,
    }}>{label}</div>
  )

  return (
    <div style={{
      border: `1px solid ${t.border}`,
      backgroundColor: t.bg,
      width: 360,
      height: 200,
      display: 'flex',
      flexDirection: 'column',
      overflow: 'hidden',
    }}>
      {/* window chrome */}
      <div style={{
        height: 18, backgroundColor: t.panel,
        borderBottom: `1px solid ${t.border}`,
        display: 'flex', alignItems: 'center',
        padding: '0 8px', justifyContent: 'space-between',
      }}>
        <span style={{ fontSize: 8, color: t.muted, letterSpacing: '0.04em' }}>설정 패널</span>
        <span style={{ fontSize: 9, color: t.muted }}>×</span>
      </div>

      <div style={{ flex: 1, display: 'flex' }}>
        {/* sidebar */}
        <div style={{
          width: 80, backgroundColor: t.panel,
          borderRight: `1px solid ${t.border}`,
          display: 'flex', flexDirection: 'column',
        }}>
          {/* search */}
          <div style={{
            height: 22, borderBottom: `1px solid ${t.border}`,
            padding: 4,
          }}>
            <div style={{ height: 14, backgroundColor: t.input, border: `1px solid ${t.inputBorder}` }} />
          </div>
          {/* nav groups */}
          {[['설정', ['일반', '언어', '터미널', '성능']], ['관리', ['자격증명', '환경 프로필']], ['모니터', ['활동', '알림']]].map(([group, items]) => (
            <div key={group as string}>
              <div style={{ fontSize: 7, color: t.muted, padding: '4px 4px 1px', letterSpacing: '0.06em' }}>{group as string}</div>
              {(items as string[]).map(item => (
                <div key={item} style={{
                  fontSize: 7, padding: '2px 4px',
                  color: item === '환경 프로필' ? t.navActiveText : t.muted,
                  backgroundColor: item === '환경 프로필' ? t.navActive : 'transparent',
                }}>{item}</div>
              ))}
            </div>
          ))}
        </div>

        {/* content */}
        <div style={{ flex: 1, display: 'flex', flexDirection: 'column' }}>
          {/* content header */}
          <div style={{
            height: 18, backgroundColor: t.surface,
            borderBottom: `1px solid ${t.border}`,
            display: 'flex', alignItems: 'center',
            padding: '0 6px', justifyContent: 'space-between',
          }}>
            <span style={{ fontSize: 7, color: t.text }}>환경 프로필</span>
            <div style={{ display: 'flex', gap: 1 }}>
              <span style={{ fontSize: 6, padding: '1px 4px', backgroundColor: t.input, border: `1px solid ${t.border}`, color: t.muted }}>라이트</span>
              <span style={{ fontSize: 6, padding: '1px 4px', backgroundColor: t.accent, color: t.accentText }}>다크</span>
            </div>
          </div>

          {/* two-column content */}
          <div style={{ flex: 1, display: 'flex' }}>
            {/* project list column */}
            <div style={{
              width: 72, backgroundColor: t.panel,
              borderRight: `1px solid ${t.border}`,
              padding: 4,
            }}>
              <div style={{ fontSize: 7, color: t.muted, marginBottom: 3, letterSpacing: '0.04em' }}>프로젝트</div>
              {['my-backend', 'frontend', 'data-pipe'].map((n, i) => (
                <div key={n} style={{
                  fontSize: 7, padding: '2px 3px', marginBottom: 1,
                  backgroundColor: i === 0 ? t.navActive : 'transparent',
                  border: `1px solid ${t.border}`,
                  color: i === 0 ? t.text : t.muted,
                  display: 'flex', justifyContent: 'space-between',
                }}>
                  <span>▤ {n}</span>
                  <span style={{ color: t.muted }}>{3 - i}e</span>
                </div>
              ))}
            </div>

            {/* detail column */}
            <div style={{ flex: 1, backgroundColor: t.surface, padding: 4, overflow: 'hidden' }}>
              <div style={{ fontSize: 7, fontWeight: 700, color: t.text, marginBottom: 2 }}>my-backend</div>
              <div style={{ fontSize: 6, color: t.muted, marginBottom: 6 }}>~/projects/my-backend</div>
              <div style={{ fontSize: 7, fontWeight: 600, color: t.text, borderBottom: `1px solid ${t.border}`, paddingBottom: 2, marginBottom: 3 }}>환경 변수 <span style={{ backgroundColor: t.tag, color: t.tagText, padding: '0 3px', fontSize: 6 }}>4</span></div>
              {['DATABASE_URL', 'REDIS_URL', 'SECRET_KEY'].map(k => (
                <div key={k} style={{ display: 'flex', gap: 4, fontSize: 6, color: t.muted, padding: '1px 0' }}>
                  <span style={{ color: t.accent, width: 56 }}>{k}</span>
                  <span>···</span>
                </div>
              ))}
              <div style={{ fontSize: 7, fontWeight: 600, color: t.text, borderBottom: `1px solid ${t.border}`, paddingBottom: 2, marginBottom: 3, marginTop: 6 }}>API 키 <span style={{ backgroundColor: t.tag, color: t.tagText, padding: '0 3px', fontSize: 6 }}>2</span></div>
              {['openai', 'github'].map(k => (
                <div key={k} style={{ display: 'flex', gap: 4, fontSize: 6, color: t.muted, padding: '1px 0' }}>
                  <span style={{ color: t.textSecondary, width: 32 }}>{k}</span>
                  <span style={{ backgroundColor: t.tag, color: t.tagText, padding: '0 2px', fontSize: 5 }}>api_key</span>
                  <span>sk-••••</span>
                </div>
              ))}
            </div>
          </div>
        </div>
      </div>
    </div>
  )
}

// ─── one theme column ─────────────────────────────────────────────────────────

function ThemeColumn({ theme }: { theme: Theme }) {
  const t = egui(theme)
  const isDark = theme === 'dark'

  return (
    <div style={{
      flex: 1,
      backgroundColor: t.surface,
      border: `1px solid ${t.border}`,
      padding: 20,
      minWidth: 0,
    }}>
      {/* Theme label */}
      <div style={{
        display: 'flex', alignItems: 'center', gap: 10, marginBottom: 20,
        paddingBottom: 10, borderBottom: `2px solid ${t.accent}`,
      }}>
        <span style={{
          fontSize: 11, fontWeight: 700, color: t.text, letterSpacing: '0.08em', textTransform: 'uppercase',
        }}>{isDark ? '다크 테마' : '라이트 테마'}</span>
        <span style={{
          fontSize: 9, padding: '2px 6px',
          backgroundColor: t.accent, color: t.accentText,
          letterSpacing: '0.04em',
        }}>{isDark ? 'DARK' : 'LIGHT'}</span>
      </div>

      {/* ── 1. Color Tokens ── */}
      <SectionTitle t={t}>01 · Color Tokens</SectionTitle>
      <Label t={t}>Surface hierarchy</Label>
      <Row gap={8}>
        <Swatch color={t.bg} name="bg" t={t} />
        <Swatch color={t.panel} name="panel" t={t} />
        <Swatch color={t.surface} name="surface" t={t} />
        <Swatch color={t.surfaceHover} name="surfaceHover" t={t} />
        <Swatch color={t.input} name="input" t={t} />
      </Row>
      <Label t={t}>Text hierarchy</Label>
      <Row gap={8}>
        <Swatch color={t.text} name="text" t={t} />
        <Swatch color={t.textSecondary} name="textSec" t={t} />
        <Swatch color={t.muted} name="muted" t={t} />
      </Row>
      <Label t={t}>Interactive & status</Label>
      <Row gap={8}>
        <Swatch color={t.accent} name="accent" t={t} />
        <Swatch color={t.accentHover} name="accentHov" t={t} />
        <Swatch color={t.navActive} name="navActive" t={t} />
        <Swatch color={t.tag} name="tag" t={t} />
        <Swatch color={t.danger} name="danger" t={t} />
      </Row>
      <Label t={t}>Structure</Label>
      <Row gap={8}>
        <Swatch color={t.border} name="border" t={t} />
        <Swatch color={t.borderFocus} name="borderFocus" t={t} />
        <Swatch color={t.inputBorder} name="inputBorder" t={t} />
        <Swatch color={t.scrollbar} name="scrollbar" t={t} />
      </Row>

      {/* ── 2. Typography ── */}
      <SectionTitle t={t}>02 · Typography · JetBrains Mono</SectionTitle>
      <div style={{ display: 'flex', flexDirection: 'column', gap: 6, marginBottom: 14 }}>
        {[
          { size: 13, weight: 600, label: 'Panel header · 13px/600', sample: '환경 프로필' },
          { size: 12, weight: 600, label: 'Section title · 12px/600', sample: 'my-backend' },
          { size: 11, weight: 400, label: 'Body / row value · 11px/400', sample: 'DATABASE_URL = postgres://localhost:5432' },
          { size: 10, weight: 400, label: 'Caption / add btn · 10px/400', sample: '+ 추가    라이트    다크' },
          { size: 9, weight: 400, label: 'Label / badge · 9px/400', sample: 'SURFACE HIERARCHY    4env    api_key' },
          { size: 8, weight: 400, label: 'Micro annotation · 8px/400', sample: '#4da6c8    3px wide · no radius' },
        ].map(row => (
          <div key={row.size + row.label} style={{ display: 'flex', alignItems: 'baseline', gap: 12 }}>
            <span style={{ width: 200, fontSize: 8, color: t.muted, flexShrink: 0 }}>{row.label}</span>
            <span style={{ fontSize: row.size, fontWeight: row.weight, color: t.text }}>{row.sample}</span>
          </div>
        ))}
      </div>

      {/* ── 3. Buttons ── */}
      <SectionTitle t={t}>03 · Buttons</SectionTitle>
      <Row>
        <BtnDemo label="default" bg={t.input} border={t.border} color={t.muted} t={t} annotation="default" />
        <BtnDemo label="hover" bg={t.surfaceHover} border={t.border} color={t.text} t={t} annotation="hover" />
        <BtnDemo label="+ 추가" bg={t.accent} border={t.accent} color={t.accentText} t={t} annotation="accent / hover" />
        <BtnDemo label="삭제" bg={t.danger} border={t.danger} color={t.dangerText} t={t} annotation="danger" />
      </Row>

      {/* ── 4. Inputs ── */}
      <SectionTitle t={t}>04 · Input Fields</SectionTitle>
      <Row>
        <InputDemo placeholder="빈 입력" t={t} label="default" />
        <InputDemo value="DATABASE_URL" focused t={t} label="focused" />
        <InputDemo value="••••••••••••••" t={t} label="masked (readonly)" />
      </Row>

      {/* ── 5. Stepper ── */}
      <SectionTitle t={t}>05 · Stepper</SectionTitle>
      <Row>
        <StepperDemo value={11} t={t} label="font size" />
        <StepperDemo value={10000} t={t} label="scroll lines" />
      </Row>

      {/* ── 6. Type Toggle ── */}
      <SectionTitle t={t}>06 · Type Toggle</SectionTitle>
      <Row>
        <TypeToggleDemo active="api_key" t={t} label="api_key selected" />
        <TypeToggleDemo active="token" t={t} label="token selected" />
      </Row>

      {/* ── 7. Theme Toggle ── */}
      <SectionTitle t={t}>07 · Theme Toggle</SectionTitle>
      <Row>
        <ThemeToggleDemo active="dark" t={t} label="dark active" />
        <ThemeToggleDemo active="light" t={t} label="light active" />
      </Row>

      {/* ── 8. Badges ── */}
      <SectionTitle t={t}>08 · Badges &amp; Tags</SectionTitle>
      <Row>
        <BadgeDemo value={4} t={t} label="count (tag bg)" />
        <BadgeDemo value="api_key" t={t} label="type label" />
        <NotifBadge value={2} t={t} label="notification (accent)" />
      </Row>

      {/* ── 9. Nav Items ── */}
      <SectionTitle t={t}>09 · Nav Items</SectionTitle>
      <Row gap={8}>
        <Cell t={t} label="default">
          <NavItemDemo label="언어" state="default" icon="⊕" t={t} />
        </Cell>
        <Cell t={t} label="hover">
          <NavItemDemo label="터미널" state="hover" icon="▣" t={t} />
        </Cell>
        <Cell t={t} label="active">
          <NavItemDemo label="환경 프로필" state="active" icon="▤" t={t} />
        </Cell>
        <Cell t={t} label="badge">
          <NavItemDemo label="알림" state="default" icon="◍" badge={2} t={t} />
        </Cell>
      </Row>

      {/* ── 10. Section Header ── */}
      <SectionTitle t={t}>10 · Section Header</SectionTitle>
      <Row>
        <SectionHeaderDemo label="환경 변수" count={4} t={t} label="with count badge + add btn" />
      </Row>

      {/* ── 11. Project List Item ── */}
      <SectionTitle t={t}>11 · Project List Item</SectionTitle>
      <Row gap={8}>
        <Cell t={t} label="default">
          <ProjectItemDemo name="frontend-app" path="~/projects/frontend-app" envCount={2} keyCount={1} state="default" t={t} />
        </Cell>
        <Cell t={t} label="hover (delete visible)">
          <ProjectItemDemo name="data-pipeline" path="/opt/pipelines/data" envCount={4} keyCount={1} state="hover" t={t} />
        </Cell>
        <Cell t={t} label="active">
          <ProjectItemDemo name="my-backend" path="~/projects/my-backend" envCount={4} keyCount={2} state="active" t={t} />
        </Cell>
      </Row>

      {/* ── 12. Env Var Row ── */}
      <SectionTitle t={t}>12 · Env Var Row</SectionTitle>
      <Row gap={8}>
        <EnvRowDemo keyName="DATABASE_URL" value="postgres://localhost" t={t} label="default" />
        <EnvRowDemo keyName="SECRET_KEY" value="sk-dev-abc123" masked t={t} label="masked" />
        <EnvRowDemo keyName="SECRET_KEY" value="sk-dev-abc123" masked revealed t={t} label="revealed" />
        <EnvRowDemo keyName="PORT" value="8080" hover t={t} label="hover state" />
      </Row>

      {/* ── 13. API Key Row ── */}
      <SectionTitle t={t}>13 · API Key Row</SectionTitle>
      <Row gap={8}>
        <ApiRowDemo provider="openai" label="dev key" type="api_key" secret="sk-••••••••••••" t={t} label="api_key" />
        <ApiRowDemo provider="github" label="personal" type="token" secret="ghp_•••••••••" hover t={t} label="token / hover" />
      </Row>

      {/* ── 14. Scrollbar ── */}
      <SectionTitle t={t}>14 · Scrollbar</SectionTitle>
      <Row>
        <ScrollbarDemo t={t} label="3px solid thumb" />
      </Row>
    </div>
  )
}

// ─── layout section ───────────────────────────────────────────────────────────

function LayoutSection({ theme }: { theme: Theme }) {
  const t = egui(theme)
  return (
    <div style={{
      backgroundColor: t.surface,
      border: `1px solid ${t.border}`,
      padding: 20,
      marginTop: 12,
    }}>
      <SectionTitle t={t}>00 · Layout Structure</SectionTitle>
      <div style={{ display: 'flex', gap: 24, flexWrap: 'wrap' }}>
        <div>
          <Label t={t}>Window — 860 × 560px</Label>
          <LayoutDiagram t={t} />
        </div>
        <div style={{ flex: 1, minWidth: 240 }}>
          <Label t={t}>Component hierarchy</Label>
          <div style={{ fontSize: 11, color: t.text, lineHeight: 2, fontFamily: 'inherit' }}>
            {[
              ['Window', t.text, 0],
              ['├─ Sidebar (190px)', t.textSecondary, 1],
              ['│  ├─ SearchInput', t.muted, 2],
              ['│  └─ NavGroup × 3', t.muted, 2],
              ['│     └─ NavItem', t.muted, 3],
              ['└─ ContentArea', t.textSecondary, 1],
              ['   ├─ HeaderBar (36px)', t.muted, 2],
              ['   │  └─ ThemeToggle', t.muted, 3],
              ['   └─ EnvProfilePanel', t.muted, 2],
              ['      ├─ ProjectList (188px)', t.muted, 3],
              ['      │  └─ ProjectItem × N', t.muted, 4],
              ['      └─ ProjectDetail', t.muted, 3],
              ['         ├─ NamePathEditor', t.muted, 4],
              ['         ├─ SectionHeader + EnvVarTable', t.muted, 4],
              ['         └─ SectionHeader + ApiKeyTable', t.muted, 4],
            ].map(([text, color, depth]) => (
              <div key={text as string} style={{
                color: color as string,
                paddingLeft: (depth as number) * 0,
                fontSize: 11,
              }}>{text}</div>
            ))}
          </div>

          <div style={{ marginTop: 16 }}>
            <Label t={t}>Spacing scale (px)</Label>
            <div style={{ display: 'flex', gap: 4, alignItems: 'flex-end' }}>
              {[2, 4, 6, 8, 10, 12, 14, 16, 20, 24].map(n => (
                <div key={n} style={{ display: 'flex', flexDirection: 'column', alignItems: 'center', gap: 3 }}>
                  <div style={{ width: n, height: n, backgroundColor: t.accent, flexShrink: 0 }} />
                  <span style={{ fontSize: 7, color: t.muted }}>{n}</span>
                </div>
              ))}
            </div>
          </div>

          <div style={{ marginTop: 16 }}>
            <Label t={t}>Border rules (egui constraints)</Label>
            <div style={{ fontSize: 10, color: t.muted, lineHeight: 1.8 }}>
              <div>• 1px solid — <span style={{ color: t.text }}>모든 경계선</span></div>
              <div>• 0px radius — <span style={{ color: t.text }}>기본 (sharp)</span></div>
              <div>• max 3px radius — <span style={{ color: t.text }}>적용 안 함</span></div>
              <div>• hover — <span style={{ color: t.text }}>background-color 전환만</span></div>
              <div>• 창 그림자 — <span style={{ color: t.text }}>box-shadow 허용 (외부만)</span></div>
              <div>• 금지 — <span style={{ color: t.danger }}>그라디언트, blur, inline shadow, 배경이미지</span></div>
            </div>
          </div>
        </div>
      </div>
    </div>
  )
}

// ─── root ─────────────────────────────────────────────────────────────────────

export default function ComponentGuide() {
  const [guideTheme, setGuideTheme] = useState<Theme>('dark')
  const t = egui(guideTheme)

  return (
    <div style={{
      width: '100%',
      minHeight: '100vh',
      backgroundColor: t.bg,
      fontFamily: '"JetBrains Mono", "Fira Mono", "Consolas", monospace',
      padding: 24,
      boxSizing: 'border-box',
    }}>
      {/* Guide header */}
      <div style={{
        display: 'flex',
        alignItems: 'center',
        justifyContent: 'space-between',
        marginBottom: 16,
        paddingBottom: 12,
        borderBottom: `1px solid ${t.border}`,
      }}>
        <div>
          <div style={{ fontSize: 16, fontWeight: 700, color: t.text, marginBottom: 2 }}>
            환경 프로필 · Component Guide
          </div>
          <div style={{ fontSize: 10, color: t.muted }}>
            egui constraints · monospace grid · 두 테마 병렬 표시
          </div>
        </div>
        <div style={{ display: 'flex', gap: 6, alignItems: 'center' }}>
          <span style={{ fontSize: 10, color: t.muted }}>가이드 배경</span>
          <div style={{ display: 'flex', border: `1px solid ${t.border}`, gap: 1 }}>
            {(['light', 'dark'] as Theme[]).map(th => (
              <button
                key={th}
                onClick={() => setGuideTheme(th)}
                style={{
                  padding: '3px 10px', fontSize: 10, fontFamily: 'inherit',
                  cursor: 'pointer', border: 'none',
                  backgroundColor: guideTheme === th ? t.accent : t.input,
                  color: guideTheme === th ? t.accentText : t.muted,
                  transition: 'background-color 0.08s',
                }}
              >{th === 'light' ? '라이트' : '다크'}</button>
            ))}
          </div>
        </div>
      </div>

      {/* Layout diagram — full width */}
      <LayoutSection theme={guideTheme} />

      {/* Two-column component guide */}
      <div style={{ display: 'flex', gap: 12, marginTop: 12 }}>
        <ThemeColumn theme="dark" />
        <ThemeColumn theme="light" />
      </div>
    </div>
  )
}
