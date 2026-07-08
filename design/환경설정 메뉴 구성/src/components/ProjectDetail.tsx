import { useState } from 'react'
import { egui, type Theme } from './egui'
import type { Project, EnvVar, ApiKey } from './EnvProfilePanel'

interface Props {
  theme: Theme
  project: Project
  onChange: (p: Project) => void
}

export default function ProjectDetail({ theme, project, onChange }: Props) {
  const t = egui(theme)
  const [editingName, setEditingName] = useState(false)
  const [editingPath, setEditingPath] = useState(false)
  const [nameVal, setNameVal] = useState(project.name)
  const [pathVal, setPathVal] = useState(project.path)

  // Reset local state when project changes
  if (nameVal !== project.name && !editingName) setNameVal(project.name)
  if (pathVal !== project.path && !editingPath) setPathVal(project.path)

  function commitName() {
    onChange({ ...project, name: nameVal })
    setEditingName(false)
  }
  function commitPath() {
    onChange({ ...project, path: pathVal })
    setEditingPath(false)
  }

  function addEnvVar() {
    const newVar: EnvVar = { id: String(Date.now()), key: 'NEW_VAR', value: '', masked: false }
    onChange({ ...project, envVars: [...project.envVars, newVar] })
  }

  function updateEnvVar(id: string, field: keyof EnvVar, value: string | boolean) {
    onChange({
      ...project,
      envVars: project.envVars.map(v => v.id === id ? { ...v, [field]: value } : v),
    })
  }

  function deleteEnvVar(id: string) {
    onChange({ ...project, envVars: project.envVars.filter(v => v.id !== id) })
  }

  function addApiKey() {
    const newKey: ApiKey = { id: String(Date.now()), provider: '', label: '새 키', type: 'api_key', secret: '' }
    onChange({ ...project, apiKeys: [...project.apiKeys, newKey] })
  }

  function updateApiKey(id: string, field: keyof ApiKey, value: string) {
    onChange({
      ...project,
      apiKeys: project.apiKeys.map(k => k.id === id ? { ...k, [field]: value } : k),
    })
  }

  function deleteApiKey(id: string) {
    onChange({ ...project, apiKeys: project.apiKeys.filter(k => k.id !== id) })
  }

  const inputStyle = {
    fontSize: 11,
    fontFamily: 'inherit',
    backgroundColor: t.input,
    border: `1px solid ${t.inputBorder}`,
    color: t.text,
    padding: '3px 6px',
    outline: 'none',
  }

  return (
    <div style={{ flex: 1, display: 'flex', flexDirection: 'column', overflow: 'hidden' }}>
      {/* Project header */}
      <div style={{
        padding: '10px 14px 8px',
        borderBottom: `1px solid ${t.border}`,
        flexShrink: 0,
      }}>
        {/* Name row */}
        <div style={{ display: 'flex', alignItems: 'center', gap: 6, marginBottom: 4 }}>
          <span style={{ fontSize: 10, color: t.muted, width: 34, flexShrink: 0 }}>이름</span>
          {editingName ? (
            <input
              autoFocus
              value={nameVal}
              onChange={e => setNameVal(e.target.value)}
              onBlur={commitName}
              onKeyDown={e => { if (e.key === 'Enter') commitName(); if (e.key === 'Escape') { setNameVal(project.name); setEditingName(false) } }}
              style={{ ...inputStyle, flex: 1 }}
            />
          ) : (
            <span
              onClick={() => setEditingName(true)}
              style={{ fontSize: 12, fontWeight: 600, color: t.text, cursor: 'text', flex: 1 }}
            >
              {project.name}
            </span>
          )}
        </div>
        {/* Path row */}
        <div style={{ display: 'flex', alignItems: 'center', gap: 6 }}>
          <span style={{ fontSize: 10, color: t.muted, width: 34, flexShrink: 0 }}>경로</span>
          {editingPath ? (
            <input
              autoFocus
              value={pathVal}
              onChange={e => setPathVal(e.target.value)}
              onBlur={commitPath}
              onKeyDown={e => { if (e.key === 'Enter') commitPath(); if (e.key === 'Escape') { setPathVal(project.path); setEditingPath(false) } }}
              style={{ ...inputStyle, flex: 1 }}
            />
          ) : (
            <span
              onClick={() => setEditingPath(true)}
              style={{ fontSize: 11, color: t.muted, cursor: 'text', flex: 1, letterSpacing: '0.01em' }}
            >
              {project.path}
            </span>
          )}
        </div>
      </div>

      {/* Scrollable body */}
      <div style={{ flex: 1, overflowY: 'auto', padding: '0 14px 12px' }}>

        {/* ENV VARS section */}
        <SectionHeader label="환경 변수" count={project.envVars.length} onAdd={addEnvVar} theme={theme} />
        <EnvVarTable
          theme={theme}
          vars={project.envVars}
          onUpdate={updateEnvVar}
          onDelete={deleteEnvVar}
        />

        {/* API KEYS section */}
        <SectionHeader label="API 키" count={project.apiKeys.length} onAdd={addApiKey} theme={theme} />
        <ApiKeyTable
          theme={theme}
          keys={project.apiKeys}
          onUpdate={updateApiKey}
          onDelete={deleteApiKey}
        />
      </div>
    </div>
  )
}

function SectionHeader({ label, count, onAdd, theme }: { label: string; count: number; onAdd: () => void; theme: Theme }) {
  const t = egui(theme)
  const [hover, setHover] = useState(false)
  return (
    <div style={{
      display: 'flex',
      alignItems: 'center',
      justifyContent: 'space-between',
      padding: '10px 0 4px',
      borderBottom: `1px solid ${t.border}`,
      marginBottom: 0,
    }}>
      <div style={{ display: 'flex', alignItems: 'center', gap: 6 }}>
        <span style={{ fontSize: 11, fontWeight: 600, color: t.text, letterSpacing: '0.02em' }}>{label}</span>
        <span style={{
          fontSize: 9,
          padding: '1px 5px',
          backgroundColor: t.tag,
          color: t.tagText,
          letterSpacing: '0.02em',
        }}>{count}</span>
      </div>
      <button
        onClick={onAdd}
        onMouseEnter={() => setHover(true)}
        onMouseLeave={() => setHover(false)}
        style={{
          fontSize: 10,
          padding: '2px 8px',
          fontFamily: 'inherit',
          cursor: 'pointer',
          backgroundColor: hover ? t.accent : t.input,
          border: `1px solid ${hover ? t.accent : t.border}`,
          color: hover ? t.accentText : t.muted,
          transition: 'background-color 0.08s, color 0.08s, border-color 0.08s',
        }}
      >+ 추가</button>
    </div>
  )
}

function EnvVarTable({ theme, vars, onUpdate, onDelete }: {
  theme: Theme
  vars: EnvVar[]
  onUpdate: (id: string, field: keyof EnvVar, value: string | boolean) => void
  onDelete: (id: string) => void
}) {
  const t = egui(theme)
  const [hoveredRow, setHoveredRow] = useState<string | null>(null)
  const [revealed, setRevealed] = useState<Set<string>>(new Set())

  function toggleReveal(id: string) {
    setRevealed(s => {
      const n = new Set(s)
      n.has(id) ? n.delete(id) : n.add(id)
      return n
    })
  }

  if (vars.length === 0) {
    return (
      <div style={{ padding: '8px 0', fontSize: 11, color: t.muted }}>
        저장된 환경 변수가 없습니다.
      </div>
    )
  }

  const inputBase = {
    fontSize: 11,
    fontFamily: 'inherit',
    backgroundColor: 'transparent',
    border: 'none',
    borderBottom: `1px solid transparent`,
    color: t.text,
    padding: '1px 2px',
    outline: 'none',
    width: '100%',
  }

  return (
    <div style={{ marginBottom: 2 }}>
      {/* Table header */}
      <div style={{
        display: 'grid',
        gridTemplateColumns: '1fr 1fr 24px 24px',
        gap: 4,
        padding: '4px 0 2px',
        borderBottom: `1px solid ${t.border}`,
      }}>
        {['키', '값', '', ''].map((h, i) => (
          <span key={i} style={{ fontSize: 9, color: t.muted, letterSpacing: '0.05em', textTransform: 'uppercase' }}>{h}</span>
        ))}
      </div>

      {vars.map(v => {
        const isHover = hoveredRow === v.id
        const show = revealed.has(v.id)
        return (
          <div
            key={v.id}
            onMouseEnter={() => setHoveredRow(v.id)}
            onMouseLeave={() => setHoveredRow(null)}
            style={{
              display: 'grid',
              gridTemplateColumns: '1fr 1fr 24px 24px',
              gap: 4,
              alignItems: 'center',
              padding: '2px 0',
              borderBottom: `1px solid ${t.border}`,
              backgroundColor: isHover ? t.surfaceHover : 'transparent',
              transition: 'background-color 0.06s',
            }}
          >
            <input
              value={v.key}
              onChange={e => onUpdate(v.id, 'key', e.target.value)}
              style={{ ...inputBase, color: t.accent, letterSpacing: '0.02em' }}
            />
            <input
              value={v.masked && !show ? '••••••••••••••••' : v.value}
              onChange={e => onUpdate(v.id, 'value', e.target.value)}
              style={{ ...inputBase }}
              readOnly={v.masked && !show}
            />
            {/* Mask toggle */}
            <button
              onClick={() => v.masked ? toggleReveal(v.id) : onUpdate(v.id, 'masked', true)}
              title={v.masked ? (show ? '숨기기' : '보기') : '마스킹'}
              style={{
                width: 22,
                height: 18,
                cursor: 'pointer',
                background: 'transparent',
                border: `1px solid ${isHover ? t.border : 'transparent'}`,
                color: v.masked ? t.accent : t.muted,
                fontSize: 10,
                fontFamily: 'inherit',
                display: 'flex',
                alignItems: 'center',
                justifyContent: 'center',
              }}
            >{v.masked ? (show ? '○' : '●') : '○'}</button>
            {/* Delete */}
            <button
              onClick={() => onDelete(v.id)}
              style={{
                width: 22,
                height: 18,
                cursor: 'pointer',
                background: 'transparent',
                border: `1px solid ${isHover ? t.border : 'transparent'}`,
                color: t.muted,
                fontSize: 11,
                fontFamily: 'inherit',
                display: 'flex',
                alignItems: 'center',
                justifyContent: 'center',
              }}
            >×</button>
          </div>
        )
      })}
    </div>
  )
}

function ApiKeyTable({ theme, keys, onUpdate, onDelete }: {
  theme: Theme
  keys: ApiKey[]
  onUpdate: (id: string, field: keyof ApiKey, value: string) => void
  onDelete: (id: string) => void
}) {
  const t = egui(theme)
  const [hoveredRow, setHoveredRow] = useState<string | null>(null)
  const [revealed, setRevealed] = useState<Set<string>>(new Set())

  function toggleReveal(id: string) {
    setRevealed(s => {
      const n = new Set(s)
      n.has(id) ? n.delete(id) : n.add(id)
      return n
    })
  }

  if (keys.length === 0) {
    return (
      <div style={{ padding: '8px 0', fontSize: 11, color: t.muted }}>
        저장된 API 키가 없습니다.
      </div>
    )
  }

  const inputBase = {
    fontSize: 11,
    fontFamily: 'inherit',
    backgroundColor: 'transparent',
    border: 'none',
    color: t.text,
    padding: '1px 2px',
    outline: 'none',
    width: '100%',
  }

  return (
    <div style={{ marginBottom: 2 }}>
      {/* Header */}
      <div style={{
        display: 'grid',
        gridTemplateColumns: '80px 1fr 56px 1fr 24px',
        gap: 4,
        padding: '4px 0 2px',
        borderBottom: `1px solid ${t.border}`,
      }}>
        {['공급자', '라벨', '종류', '비밀키', ''].map((h, i) => (
          <span key={i} style={{ fontSize: 9, color: t.muted, letterSpacing: '0.05em', textTransform: 'uppercase' }}>{h}</span>
        ))}
      </div>

      {keys.map(k => {
        const isHover = hoveredRow === k.id
        const show = revealed.has(k.id)
        return (
          <div
            key={k.id}
            onMouseEnter={() => setHoveredRow(k.id)}
            onMouseLeave={() => setHoveredRow(null)}
            style={{
              display: 'grid',
              gridTemplateColumns: '80px 1fr 56px 1fr 24px',
              gap: 4,
              alignItems: 'center',
              padding: '2px 0',
              borderBottom: `1px solid ${t.border}`,
              backgroundColor: isHover ? t.surfaceHover : 'transparent',
              transition: 'background-color 0.06s',
            }}
          >
            <input
              value={k.provider}
              onChange={e => onUpdate(k.id, 'provider', e.target.value)}
              style={{ ...inputBase, color: t.textSecondary }}
            />
            <input
              value={k.label}
              onChange={e => onUpdate(k.id, 'label', e.target.value)}
              style={{ ...inputBase }}
            />
            {/* Type toggle */}
            <button
              onClick={() => onUpdate(k.id, 'type', k.type === 'api_key' ? 'token' : 'api_key')}
              style={{
                fontSize: 9,
                padding: '2px 5px',
                cursor: 'pointer',
                fontFamily: 'inherit',
                backgroundColor: k.type === 'token' ? t.accent : t.input,
                border: `1px solid ${k.type === 'token' ? t.accent : t.border}`,
                color: k.type === 'token' ? t.accentText : t.muted,
                letterSpacing: '0.02em',
                transition: 'background-color 0.08s',
              }}
            >{k.type}</button>
            {/* Secret */}
            <div style={{ display: 'flex', alignItems: 'center', gap: 2 }}>
              <input
                value={show ? k.secret.replace(/•/g, '') || k.secret : k.secret}
                onChange={e => onUpdate(k.id, 'secret', e.target.value)}
                type={show ? 'text' : 'password'}
                style={{ ...inputBase, flex: 1 }}
              />
              <button
                onClick={() => toggleReveal(k.id)}
                style={{
                  width: 18,
                  height: 16,
                  cursor: 'pointer',
                  background: 'transparent',
                  border: `1px solid ${isHover ? t.border : 'transparent'}`,
                  color: show ? t.accent : t.muted,
                  fontSize: 9,
                  fontFamily: 'inherit',
                  flexShrink: 0,
                  display: 'flex',
                  alignItems: 'center',
                  justifyContent: 'center',
                }}
              >{show ? '●' : '○'}</button>
            </div>
            {/* Delete */}
            <button
              onClick={() => onDelete(k.id)}
              style={{
                width: 22,
                height: 18,
                cursor: 'pointer',
                background: 'transparent',
                border: `1px solid ${isHover ? t.border : 'transparent'}`,
                color: t.muted,
                fontSize: 11,
                fontFamily: 'inherit',
                display: 'flex',
                alignItems: 'center',
                justifyContent: 'center',
              }}
            >×</button>
          </div>
        )
      })}
    </div>
  )
}
