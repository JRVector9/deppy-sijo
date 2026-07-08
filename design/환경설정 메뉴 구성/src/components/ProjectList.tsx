import { useState } from 'react'
import { egui, type Theme } from './egui'
import type { Project } from './EnvProfilePanel'

interface Props {
  theme: Theme
  projects: Project[]
  selectedId: string
  onSelect: (id: string) => void
  onAdd: () => void
  onDelete: (id: string) => void
}

export default function ProjectList({ theme, projects, selectedId, onSelect, onAdd, onDelete }: Props) {
  const t = egui(theme)
  const [hoveredId, setHoveredId] = useState<string | null>(null)
  const [hoveredDel, setHoveredDel] = useState<string | null>(null)

  return (
    <div style={{
      width: 188,
      flexShrink: 0,
      display: 'flex',
      flexDirection: 'column',
      backgroundColor: t.panel,
    }}>
      {/* Section header */}
      <div style={{
        display: 'flex',
        alignItems: 'center',
        justifyContent: 'space-between',
        padding: '8px 10px 6px',
        borderBottom: `1px solid ${t.border}`,
      }}>
        <span style={{ fontSize: 10, color: t.muted, letterSpacing: '0.06em', textTransform: 'uppercase' }}>
          프로젝트
        </span>
        <button
          onClick={onAdd}
          onMouseEnter={() => setHoveredId('__add')}
          onMouseLeave={() => setHoveredId(null)}
          style={{
            background: hoveredId === '__add' ? t.accent : t.input,
            border: `1px solid ${t.border}`,
            color: hoveredId === '__add' ? t.accentText : t.muted,
            fontSize: 13,
            lineHeight: 1,
            cursor: 'pointer',
            width: 18,
            height: 18,
            display: 'flex',
            alignItems: 'center',
            justifyContent: 'center',
            fontFamily: 'inherit',
            transition: 'background-color 0.08s, color 0.08s',
          }}
        >+</button>
      </div>

      {/* Project items */}
      <div style={{ flex: 1, overflowY: 'auto' }}>
        {projects.map(proj => {
          const isActive = proj.id === selectedId
          const isHover = hoveredId === proj.id && !isActive
          return (
            <div
              key={proj.id}
              onClick={() => onSelect(proj.id)}
              onMouseEnter={() => setHoveredId(proj.id)}
              onMouseLeave={() => setHoveredId(null)}
              style={{
                display: 'flex',
                alignItems: 'center',
                gap: 6,
                padding: '6px 10px',
                cursor: 'pointer',
                backgroundColor: isActive ? t.navActive : isHover ? t.surfaceHover : 'transparent',
                borderBottom: `1px solid ${t.border}`,
                transition: 'background-color 0.08s',
                position: 'relative',
              }}
            >
              {/* Folder icon */}
              <span style={{ fontSize: 11, color: isActive ? t.accent : t.muted, flexShrink: 0 }}>▤</span>

              {/* Project info */}
              <div style={{ flex: 1, minWidth: 0 }}>
                <div style={{
                  fontSize: 11,
                  fontWeight: isActive ? 600 : 400,
                  color: isActive ? t.text : t.textSecondary,
                  overflow: 'hidden',
                  textOverflow: 'ellipsis',
                  whiteSpace: 'nowrap',
                }}>
                  {proj.name}
                </div>
                <div style={{
                  fontSize: 9,
                  color: t.muted,
                  overflow: 'hidden',
                  textOverflow: 'ellipsis',
                  whiteSpace: 'nowrap',
                  marginTop: 1,
                  letterSpacing: '0.01em',
                }}>
                  {proj.path}
                </div>
              </div>

              {/* Counts */}
              <div style={{ display: 'flex', flexDirection: 'column', alignItems: 'flex-end', gap: 2, flexShrink: 0 }}>
                <span style={{ fontSize: 9, color: t.muted }}>{proj.envVars.length}env</span>
                <span style={{ fontSize: 9, color: t.muted }}>{proj.apiKeys.length}key</span>
              </div>

              {/* Delete button (hover only) */}
              {(isHover || isActive) && (
                <button
                  onClick={e => { e.stopPropagation(); onDelete(proj.id) }}
                  onMouseEnter={() => setHoveredDel(proj.id)}
                  onMouseLeave={() => setHoveredDel(null)}
                  style={{
                    position: 'absolute',
                    right: 4,
                    top: 4,
                    width: 14,
                    height: 14,
                    display: 'flex',
                    alignItems: 'center',
                    justifyContent: 'center',
                    background: hoveredDel === proj.id ? t.danger : 'transparent',
                    border: `1px solid ${hoveredDel === proj.id ? t.danger : t.border}`,
                    color: hoveredDel === proj.id ? t.dangerText : t.muted,
                    cursor: 'pointer',
                    fontSize: 9,
                    fontFamily: 'inherit',
                    transition: 'background-color 0.08s',
                  }}
                >×</button>
              )}
            </div>
          )
        })}

        {projects.length === 0 && (
          <div style={{ padding: 12, fontSize: 11, color: t.muted, textAlign: 'center' }}>
            프로젝트 없음
          </div>
        )}
      </div>
    </div>
  )
}
