import { useState } from 'react'
import { egui, type Theme } from './egui'
import ProjectList from './ProjectList'
import ProjectDetail from './ProjectDetail'

export interface EnvVar {
  id: string
  key: string
  value: string
  masked: boolean
}

export interface ApiKey {
  id: string
  provider: string
  label: string
  type: 'api_key' | 'token'
  secret: string
}

export interface Project {
  id: string
  name: string
  path: string
  envVars: EnvVar[]
  apiKeys: ApiKey[]
}

const INITIAL_PROJECTS: Project[] = [
  {
    id: '1',
    name: 'my-backend',
    path: '~/projects/my-backend',
    envVars: [
      { id: 'e1', key: 'DATABASE_URL', value: 'postgres://localhost:5432/dev', masked: false },
      { id: 'e2', key: 'REDIS_URL', value: 'redis://localhost:6379', masked: false },
      { id: 'e3', key: 'SECRET_KEY', value: 'sk-dev-••••••••••••••••', masked: true },
      { id: 'e4', key: 'PORT', value: '8080', masked: false },
    ],
    apiKeys: [
      { id: 'k1', provider: 'openai', label: 'dev key', type: 'api_key', secret: 'sk-••••••••••••••••••••' },
      { id: 'k2', provider: 'github', label: 'personal token', type: 'token', secret: 'ghp_••••••••••••••' },
    ],
  },
  {
    id: '2',
    name: 'frontend-app',
    path: '~/projects/frontend-app',
    envVars: [
      { id: 'e5', key: 'VITE_API_URL', value: 'http://localhost:3000', masked: false },
      { id: 'e6', key: 'VITE_APP_TITLE', value: 'My App', masked: false },
    ],
    apiKeys: [
      { id: 'k3', provider: 'vercel', label: 'deploy token', type: 'token', secret: 'vt_••••••••••••' },
    ],
  },
  {
    id: '3',
    name: 'data-pipeline',
    path: '/opt/pipelines/data',
    envVars: [
      { id: 'e7', key: 'SPARK_MASTER', value: 'spark://cluster:7077', masked: false },
      { id: 'e8', key: 'S3_BUCKET', value: 'my-data-lake', masked: false },
      { id: 'e9', key: 'AWS_SECRET', value: '••••••••••••••••••••••••••••••••', masked: true },
      { id: 'e10', key: 'AWS_KEY_ID', value: 'AKIAIOSFODNN7EXAMPLE', masked: false },
    ],
    apiKeys: [
      { id: 'k4', provider: 'aws', label: 'prod credentials', type: 'api_key', secret: 'AKIA••••••••••••' },
    ],
  },
]

export default function EnvProfilePanel({ theme }: { theme: Theme }) {
  const t = egui(theme)
  const [projects, setProjects] = useState<Project[]>(INITIAL_PROJECTS)
  const [selectedId, setSelectedId] = useState<string>('1')

  const selected = projects.find(p => p.id === selectedId) ?? null

  function updateProject(updated: Project) {
    setProjects(ps => ps.map(p => p.id === updated.id ? updated : p))
  }

  function addProject() {
    const id = String(Date.now())
    const newProject: Project = {
      id,
      name: '새 프로젝트',
      path: '~/projects/new-project',
      envVars: [],
      apiKeys: [],
    }
    setProjects(ps => [...ps, newProject])
    setSelectedId(id)
  }

  function deleteProject(id: string) {
    setProjects(ps => ps.filter(p => p.id !== id))
    if (selectedId === id) {
      const remaining = projects.filter(p => p.id !== id)
      setSelectedId(remaining[0]?.id ?? '')
    }
  }

  return (
    <div style={{ display: 'flex', height: '100%' }}>
      {/* Project list column */}
      <ProjectList
        theme={theme}
        projects={projects}
        selectedId={selectedId}
        onSelect={setSelectedId}
        onAdd={addProject}
        onDelete={deleteProject}
      />

      {/* Divider */}
      <div style={{ width: 1, backgroundColor: t.border, flexShrink: 0 }} />

      {/* Project detail column */}
      {selected ? (
        <ProjectDetail
          theme={theme}
          project={selected}
          onChange={updateProject}
        />
      ) : (
        <div style={{ flex: 1, display: 'flex', alignItems: 'center', justifyContent: 'center', color: t.muted, fontSize: 12 }}>
          프로젝트를 선택하세요
        </div>
      )}
    </div>
  )
}
