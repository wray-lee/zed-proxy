// Shared visual blocks: loading, empty, error and the global toast.

import { h, icon } from './dom.ts'
import { describe, failureFrom } from './api.ts'
import type { Failure } from './api.ts'
import { state } from './state.ts'
import type { TextKey } from './text.ts'
import { tr } from './text.ts'

export type Tone = 'ok' | 'warn' | 'bad' | 'idle'

let toastTimer: number | undefined

export function toast(message: string, tone: Tone = 'idle'): void {
  const host = document.getElementById('toast')
  if (!host) return
  host.replaceChildren(
    h(
      'div',
      { class: 'toast-body' },
      h('span', { class: 'toast-text' }, message),
    ),
  )
  host.dataset.tone = tone
  host.dataset.visible = 'true'
  if (toastTimer !== undefined) window.clearTimeout(toastTimer)
  toastTimer = window.setTimeout(() => {
    host.dataset.visible = 'false'
    host.replaceChildren()
  }, 7000)
}

export function loadingBlock(label?: string): HTMLElement {
  return h(
    'div',
    { class: 'loading', role: 'status' },
    h('span', { class: 'spinner', 'aria-hidden': 'true' }),
    h('p', {}, label ?? tr('loadingDetail', state.lang)),
  )
}

export function emptyBlock(title: string, body?: string, action?: Node): HTMLElement {
  return h(
    'div',
    { class: 'empty' },
    h('p', { class: 'empty-title' }, title),
    body ? h('p', {}, body) : null,
    action ?? null,
  )
}

export interface ErrorBlockOptions {
  titleKey: TextKey | null
  title?: string
  failure: Failure
  onRetry: () => void
  /** Timestamp text of the stale data still on screen. */
  staleSince?: string | null
}

export function errorBlock(options: ErrorBlockOptions): HTMLElement {
  const lang = state.lang
  const title = options.title ?? (options.titleKey ? tr(options.titleKey, lang) : tr('errorUnknown', lang))
  const children: (Node | null)[] = [
    h('p', { class: 'error-title' }, title),
    h('p', {}, describe(options.failure, lang)),
    options.staleSince
      ? h('p', { class: 'detail-note' }, staleText(options.staleSince))
      : null,
    h('button', { type: 'button', class: 'btn', onclick: options.onRetry }, tr('retry', lang)),
  ]
  return h('div', { class: 'errorbox', role: 'alert' }, ...children.filter((child): child is Node => child !== null))
}

export function staleText(since: string): string {
  return tr('dataFromBefore', state.lang).replace('{0}', since)
}

export function failureOf(err: unknown): Failure {
  return failureFrom(err)
}

export function iconForTone(tone: Tone): SVGSVGElement {
  if (tone === 'ok') return icon('check')
  if (tone === 'bad') return icon('cross')
  if (tone === 'warn') return icon('warning')
  return icon('dot')
}

export function pageHead(titleId: string, title: string, lede: string, actions?: Node[]): HTMLElement {
  return h(
    'header',
    { class: 'page-head' },
    h('h1', { class: 'page-title', id: titleId, tabindex: '-1' }, title),
    h('p', { class: 'lede' }, lede),
    actions && actions.length > 0 ? h('div', { class: 'page-actions' }, ...actions) : null,
  )
}

export function section(title: string, note: string | null, ...body: (Node | null)[]): HTMLElement {
  return h(
    'section',
    { class: 'section' },
    h(
      'div',
      { class: 'section-head' },
      h('h2', { class: 'section-title' }, title),
      note ? h('p', { class: 'section-note' }, note) : null,
    ),
    ...body.filter((node): node is Node => node !== null),
  )
}

export function badge(label: string, tone: Tone): HTMLElement {
  return h('span', { class: 'badge', 'data-tone': tone }, iconForTone(tone), label)
}
