// Same-origin HTTP access. Every protected call goes through request(), which
// attaches the gateway key, merges concurrent 401 prompts into one dialog and
// retries each request exactly once.

import { h } from './dom.ts'
import { openDialog } from './dialog.ts'
import { saveApiKey, state } from './state.ts'
import { tf, tr } from './text.ts'

export class ApiError extends Error {
  readonly status: number
  readonly detail: string | null

  constructor(status: number, detail: string | null) {
    super(`HTTP ${status}`)
    this.name = 'ApiError'
    this.status = status
    this.detail = detail
  }
}

export interface Failure {
  kind: 'network' | 'status'
  status: number
  message: string
  /** Server text worth showing next to the message, already sanitized. */
  detail: string | null
}

const MACHINE_CODE = /^[a-z0-9_]+$/i

function humanDetail(text: string | null): string | null {
  if (!text) return null
  if (MACHINE_CODE.test(text)) return null
  return text.length > 240 ? `${text.slice(0, 240)}…` : text
}

/** Pulls the management `error` string, or the nested protocol message. */
async function readErrorBody(response: Response): Promise<string | null> {
  try {
    const text = await response.text()
    if (text.trim() === '') return null
    const parsed: unknown = JSON.parse(text)
    if (typeof parsed === 'object' && parsed !== null) {
      const record = parsed as Record<string, unknown>
      if (typeof record.error === 'string') return record.error
      const nested = record.error
      if (typeof nested === 'object' && nested !== null) {
        const message = (nested as Record<string, unknown>).message
        if (typeof message === 'string') return message
      }
    }
  } catch {
    // Not JSON, or already consumed: fall back to the status code.
  }
  return null
}

export function describe(failure: Failure, lang: 'zh' | 'en'): string {
  if (failure.kind === 'network') return tr('errorNetwork', lang)
  const detail = failure.detail ? ` ${failure.detail}` : ''
  switch (failure.status) {
    case 400:
      return tr('errorRequestRejected', lang) + detail
    case 401:
      return tr('errorUnauthorized', lang) + detail
    case 403:
      return tr('errorForbidden', lang) + detail
    case 404:
      return tr('errorAccountMissing', lang) + detail
    case 409:
      return tr('errorLoginBusy', lang) + detail
    case 410:
      return tr('errorLoginExpired', lang) + detail
    case 413:
      return tr('errorTooLarge', lang) + detail
    case 429:
      return tr('errorRateLimited', lang) + detail
    default:
      return (failure.status >= 500 ? tr('errorServer', lang) : tf('errorStatus', lang, failure.status)) + detail
  }
}

export function failureFrom(err: unknown): Failure {
  if (err instanceof ApiError) {
    return { kind: 'status', status: err.status, message: `HTTP ${err.status}`, detail: err.detail }
  }
  return { kind: 'network', status: 0, message: String(err), detail: null }
}

// ---- Gateway key prompt -----------------------------------------------------

let pendingPrompt: Promise<boolean> | null = null

function askForKey(): Promise<boolean> {
  if (pendingPrompt) return pendingPrompt
  pendingPrompt = new Promise<boolean>((resolve) => {
    let settled = false
    const settle = (accepted: boolean): void => {
      if (settled) return
      settled = true
      resolve(accepted)
    }
    const field = h('input', {
      class: 'field-input',
      type: 'password',
      id: 'gateway-key',
      autocomplete: 'off',
      spellcheck: 'false',
      autocapitalize: 'off',
      placeholder: tr('keyPlaceholder', state.lang),
    })
    const toggle = h(
      'button',
      {
        type: 'button',
        class: 'btn btn-small',
        onclick: () => {
          const showing = field.type === 'text'
          field.type = showing ? 'password' : 'text'
          toggle.textContent = tr(showing ? 'keyShow' : 'keyHide', state.lang)
        },
      },
      tr('keyShow', state.lang),
    )
    const error = h('p', { class: 'dialog-error', role: 'alert', hidden: true })
    const handle = openDialog({
      title: tr('keyTitle', state.lang),
      body: [
        h('p', { class: 'dialog-text' }, tr('keyBody', state.lang)),
        h(
          'div',
          { class: 'field' },
          h('label', { class: 'field-label', for: 'gateway-key' }, tr('keyLabel', state.lang)),
          h('div', { class: 'field-row' }, field, toggle),
        ),
        error,
      ],
      actions: [
        {
          label: tr('keySubmit', state.lang),
          primary: true,
          onSelect: () => {
            const value = field.value.trim()
            if (value === '') {
              error.textContent = tr('keyEmpty', state.lang)
              error.hidden = false
              field.focus()
              return false
            }
            saveApiKey(value)
            settle(true)
            return true
          },
        },
      ],
      initialFocus: field,
      closeLabel: tr('dialogClose', state.lang),
      onCancel: () => settle(false),
    })
    field.addEventListener('keydown', (event) => {
      if (event.key !== 'Enter') return
      event.preventDefault()
      const value = field.value.trim()
      if (value === '') {
        error.textContent = tr('keyEmpty', state.lang)
        error.hidden = false
        return
      }
      saveApiKey(value)
      settle(true)
      handle.close()
    })
  }).finally(() => {
    pendingPrompt = null
  })
  return pendingPrompt
}

// ---- Core request -----------------------------------------------------------

interface RequestOptions {
  method?: string
  body?: unknown
  /** Session-wide catalogue reuse; callers pass explicitly. */
  signal?: AbortSignal
}

async function send(path: string, options: RequestOptions, authorized: boolean): Promise<Response> {
  const headers: Record<string, string> = { Accept: 'application/json' }
  if (authorized && state.apiKey !== '') headers.Authorization = `Bearer ${state.apiKey}`
  let body: string | undefined
  if (options.body !== undefined) {
    headers['Content-Type'] = 'application/json'
    body = JSON.stringify(options.body)
  }
  return fetch(path, {
    method: options.method ?? 'GET',
    headers,
    body,
    cache: 'no-store',
    credentials: 'omit',
    signal: options.signal,
  })
}

export async function request<T>(path: string, options: RequestOptions = {}): Promise<T> {
  const authorized = true
  let response: Response
  try {
    response = await send(path, options, authorized)
  } catch (err) {
    throw err
  }
  if (response.status === 401) {
    const accepted = await askForKey()
    if (!accepted) throw new ApiError(401, null)
    response = await send(path, options, authorized)
    if (response.status === 401) throw new ApiError(401, null)
  }
  if (!response.ok) {
    throw new ApiError(response.status, humanDetail(await readErrorBody(response)))
  }
  const text = await response.text()
  if (text.trim() === '') return {} as T
  try {
    return JSON.parse(text) as T
  } catch {
    throw new ApiError(response.status, null)
  }
}

/** Public routes never carry the key and never trigger the prompt. */
export async function requestPublic<T>(path: string, options: RequestOptions = {}): Promise<T> {
  const response = await send(path, options, false)
  if (!response.ok) throw new ApiError(response.status, humanDetail(await readErrorBody(response)))
  const text = await response.text()
  if (text.trim() === '') return {} as T
  try {
    return JSON.parse(text) as T
  } catch {
    throw new ApiError(response.status, null)
  }
}
