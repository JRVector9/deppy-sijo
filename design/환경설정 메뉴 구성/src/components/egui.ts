export type Theme = 'dark' | 'light'

export interface EguiTokens {
  bg: string
  surface: string
  surfaceHover: string
  panel: string
  border: string
  borderFocus: string
  text: string
  textSecondary: string
  muted: string
  accent: string
  accentHover: string
  accentText: string
  input: string
  inputBorder: string
  danger: string
  dangerText: string
  navActive: string
  navActiveText: string
  scrollbar: string
  tag: string
  tagText: string
}

const dark: EguiTokens = {
  bg: '#1a1a1a',
  surface: '#242424',
  surfaceHover: '#2c2c2c',
  panel: '#1e1e1e',
  border: '#3a3a3a',
  borderFocus: '#5a9fd4',
  text: '#d4d4d4',
  textSecondary: '#aaaaaa',
  muted: '#717171',
  accent: '#4da6c8',
  accentHover: '#5bbad8',
  accentText: '#ffffff',
  input: '#1a1a1a',
  inputBorder: '#404040',
  danger: '#c84d4d',
  dangerText: '#ffffff',
  navActive: '#2e4a5e',
  navActiveText: '#d4d4d4',
  scrollbar: '#3a3a3a',
  tag: '#2a3a44',
  tagText: '#4da6c8',
}

const light: EguiTokens = {
  bg: '#e0e0e0',
  surface: '#f0f0f0',
  surfaceHover: '#e8e8e8',
  panel: '#fafafa',
  border: '#c4c4c4',
  borderFocus: '#3a88bf',
  text: '#1a1a1a',
  textSecondary: '#444444',
  muted: '#888888',
  accent: '#3a88bf',
  accentHover: '#2e7aaf',
  accentText: '#ffffff',
  input: '#ffffff',
  inputBorder: '#b8b8b8',
  danger: '#bf3a3a',
  dangerText: '#ffffff',
  navActive: '#ccdeed',
  navActiveText: '#1a2a3a',
  scrollbar: '#c4c4c4',
  tag: '#d0e8f4',
  tagText: '#2a6080',
}

export function egui(theme: Theme): EguiTokens {
  return theme === 'dark' ? dark : light
}
