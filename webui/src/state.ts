// Runtime state and the tolerant storage layer. Nothing here touches the DOM
// at module scope, so a broken storage implementation cannot stop the bundle.

import type { Lang } from './text.ts'

const LANG_KEY = 'zed_lang'
const KEY_KEY = 'zed_api_key'
const PROBE_KEY = 'zed_probe_model'

export const DEFAULT_PROBE_MODEL = 'gpt-5.6-luna'
export const DEFAULT_PORT = '8001'

export function readStore(name: string): string | null {
  try {
    return window.localStorage.getItem(name)
  } catch {
    return null
  }
}

export function writeStore(name: string, value: string): void {
  try {
    window.localStorage.setItem(name, value)
  } catch {
    // Quota or a blocked site: the in-memory value stays authoritative.
  }
}

export function detectLanguage(): Lang {
  const saved = readStore(LANG_KEY)
  if (saved === 'zh' || saved === 'en') return saved
  const nav = typeof navigator === 'undefined' ? '' : navigator.language || ''
  return nav.toLowerCase().startsWith('zh') ? 'zh' : 'en'
}

export interface AppState {
  lang: Lang
  /** Gateway key for this session; storage may refuse to keep it. */
  apiKey: string
  probeModel: string
  page: PageId
}

export type PageId = 'accounts' | 'models' | 'health' | 'endpoints' | 'integration'

export const PAGES: readonly PageId[] = ['accounts', 'models', 'health', 'endpoints', 'integration']

export function isPage(value: string): value is PageId {
  return (PAGES as readonly string[]).includes(value)
}

export function pageFromHash(): PageId {
  const raw = window.location.hash.replace(/^#/, '')
  return isPage(raw) ? raw : 'accounts'
}

export const state: AppState = {
  lang: 'en',
  apiKey: '',
  probeModel: DEFAULT_PROBE_MODEL,
  page: 'accounts',
}

/** Values saved before the console knows which language it is running in. */
export function loadStoredPreferences(): void {
  const key = readStore(KEY_KEY)
  if (key !== null) state.apiKey = key
  const probe = readStore(PROBE_KEY)
  if (probe !== null && probe !== '') state.probeModel = probe
}

export function saveProbeModel(model: string): void {
  state.probeModel = model
  writeStore(PROBE_KEY, model)
}

export function saveApiKey(key: string): void {
  state.apiKey = key
  writeStore(KEY_KEY, key)
}

export function saveLanguage(lang: Lang): void {
  state.lang = lang
  writeStore(LANG_KEY, lang)
}

export function currentPort(): string {
  const port = window.location.port
  return port === '' ? DEFAULT_PORT : port
}

/** Protocol, host and port of the page, without a trailing slash. */
export function pageOrigin(): string {
  const { protocol, hostname, port } = window.location
  if (protocol === 'file:') return `http://127.0.0.1:${DEFAULT_PORT}`
  return `${protocol}//${hostname}${port === '' ? '' : `:${port}`}`
}

const listeners = new Set<() => void>()

export function announce(): void {
  for (const listener of listeners) {
    try {
      listener()
    } catch (err) {
      console.error('[console] listener failed', err)
    }
  }
}

export function subscribe(listener: () => void): void {
  listeners.add(listener)
}
