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
      width: 220,
      minWidth: 220,
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
        height: 42,
        padding: '0 10px',
        borderBottom: `1px solid ${t.border}`,
        boxSizing: 'border-box',
      }}>
        <span style={{ fontSize: 13, color: t.muted, letterSpacing: '0.06em', textTransform: 'uppercase' }}>
          프로젝트
        </span>
        <button
          onClick={onAdd}
          onMouseEnter={() => setHoveredId('__add')}
          onMouseLeave={() => setHoveredId(null)}
            style={{
              background: hoveredId === '__add' ? t.accent : t.input,
              border: `1px solid ${hoveredId === '__add' ? t.accent : t.border}`,
              color: hoveredId === '__add' ? t.accentText : t.muted,
              fontSize: 16,
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
          const showDelete = isHover || isActive
          const deleteHover = hoveredDel === proj.id
          return (
            <div
              key={proj.id}
              onClick={() => onSelect(proj.id)}
              onMouseEnter={() => setHoveredId(proj.id)}
              onMouseLeave={() => setHoveredId(null)}
              style={{
                display: 'grid',
                gridTemplateColumns: 'minmax(0, 1fr) 52px 22px',
                gridTemplateRows: '24px 22px',
                alignItems: 'center',
                columnGap: 6,
                rowGap: 2,
                minHeight: 76,
                padding: '10px',
                cursor: 'pointer',
                backgroundColor: isActive ? t.navActive : isHover ? t.surfaceHover : 'transparent',
                borderBottom: `1px solid ${t.border}`,
                transition: 'background-color 0.08s',
                boxSizing: 'border-box',
              }}
            >
              <div style={{
                gridColumn: 1,
                gridRow: 1,
                minWidth: 0,
                fontSize: 14,
                fontWeight: isActive ? 600 : 400,
                color: isActive ? t.text : t.textSecondary,
                overflow: 'hidden',
                textOverflow: 'ellipsis',
                whiteSpace: 'nowrap',
              }}>
                {proj.name}
              </div>
              <div style={{
                gridColumn: 1,
                gridRow: 2,
                minWidth: 0,
                fontSize: 12,
                color: isActive ? t.textSecondary : t.muted,
                overflow: 'hidden',
                textOverflow: 'ellipsis',
                whiteSpace: 'nowrap',
                letterSpacing: '0.01em',
              }}>
                {proj.path}
              </div>
              <span style={{
                gridColumn: 2,
                gridRow: 1,
                justifySelf: 'end',
                fontSize: 12,
                color: isActive ? t.textSecondary : t.muted,
                whiteSpace: 'nowrap',
                fontVariantNumeric: 'tabular-nums',
              }}>
                {proj.envVars.length}env
              </span>
              <span style={{
                gridColumn: 2,
                gridRow: 2,
                justifySelf: 'end',
                fontSize: 12,
                color: isActive ? t.textSecondary : t.muted,
                whiteSpace: 'nowrap',
                fontVariantNumeric: 'tabular-nums',
              }}>
                {proj.apiKeys.length}key
              </span>
              <button
                onClick={e => { e.stopPropagation(); onDelete(proj.id) }}
                onMouseEnter={() => setHoveredDel(proj.id)}
                onMouseLeave={() => setHoveredDel(null)}
                aria-label={`${proj.name} 삭제`}
                style={{
                  gridColumn: 3,
                  gridRow: 1,
                  justifySelf: 'end',
                  alignSelf: 'center',
                  width: 22,
                  height: 20,
                  display: 'flex',
                  alignItems: 'center',
                  justifyContent: 'center',
                  visibility: showDelete ? 'visible' : 'hidden',
                  pointerEvents: showDelete ? 'auto' : 'none',
                  background: deleteHover ? t.danger : 'transparent',
                  border: `1px solid ${deleteHover ? t.danger : t.border}`,
                  color: deleteHover ? t.dangerText : t.muted,
                  cursor: 'pointer',
                  fontSize: 12,
                  fontFamily: 'inherit',
                  lineHeight: 1,
                  padding: 0,
                  transition: 'background-color 0.08s, color 0.08s, border-color 0.08s',
                }}
              >×</button>
            </div>
          )
        })}

        {projects.length === 0 && (
          <div style={{ padding: 12, fontSize: 14, color: t.muted, textAlign: 'center' }}>
            프로젝트 없음
          </div>
        )}
      </div>
    </div>
  )
}
