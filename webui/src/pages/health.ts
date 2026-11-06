// Diagnostics page: six fixed read-only checks, updated one row at a time.

import { h, clear, icon, pageRoot } from '../dom.ts'
import { describe, failureFrom } from '../api.ts'
import { fetchAccounts, fetchStatus, loadCatalogue, preflight } from '../data.ts'
import type { TextKey } from '../text.ts'
import { tf, tr } from '../text.ts'
import { dateTime } from '../format.ts'
import { state } from '../state.ts'
import { pageHead } from '../ui.ts'
import { ensureCatalogue, reloadAccounts, reloadStatus, view } from '../store.ts'

type CheckState = 'pending' | 'running' | 'pass' | 'fail'

interface CheckRow {
  key: TextKey
  state: CheckState
  detail: string | null
}

const CHECKS: TextKey[] = ['check1', 'check2', 'check3', 'check4', 'check5', 'check6']

const rows: CheckRow[] = CHECKS.map((key) => ({ key, state: 'pending', detail: null }))
let running = false
let finishedAt: number | null = null
let summaryKey: TextKey = 'checksSummaryRunning'
let summaryArgs: number[] = [CHECKS.length]

const host = (): HTMLElement | null => pageRoot('health')

function setRow(index: number, state: CheckState, detail: string | null): void {
  const row = rows[index]
  if (!row) return
  row.state = state
  row.detail = detail
}

async function checkService(): Promise<void> {
  setRow(0, 'running', null)
  render()
  const started = performance.now()
  try {
    const catalogue = view.catalogue.phase === 'ready' ? view.catalogue : await loadCatalogue()
    const elapsed = Math.round(performance.now() - started)
    setRow(0, 'pass', tf('check1Pass', state.lang, catalogue.models.length, elapsed))
  } catch (err) {
    setRow(0, 'fail', describe(failureFrom(err), state.lang))
  }
  render()
}

async function checkAccounts(): Promise<void> {
  setRow(1, 'running', null)
  render()
  try {
    const result = await fetchAccounts()
    if (result.accounts.length === 0) {
      setRow(1, 'fail', tr('check2Fail', state.lang))
    } else if (result.current !== null) {
      setRow(1, 'pass', tf('check2Pass', state.lang, result.accounts.length, result.current))
    } else {
      setRow(1, 'pass', tf('check2NoActive', state.lang, result.accounts.length))
    }
  } catch (err) {
    setRow(1, 'fail', describe(failureFrom(err), state.lang))
  }
  render()
}

async function checkTokens(): Promise<void> {
  setRow(2, 'running', null)
  render()
  try {
    const result = await fetchStatus()
    const total = result.rows.length
    if (total === 0) {
      setRow(2, 'fail', tr('checkNoAccounts', state.lang))
    } else {
      let bad = 0
      for (const row of result.rows) if (row.tokenOk !== true) bad += 1
      if (bad === 0) setRow(2, 'pass', tf('check3Pass', state.lang, total))
      else setRow(2, 'fail', tf('check3Fail', state.lang, bad))
    }
  } catch (err) {
    setRow(2, 'fail', describe(failureFrom(err), state.lang))
  }
  render()
}

async function checkRoute(index: number, path: string): Promise<void> {
  setRow(index, 'running', null)
  render()
  const status = await preflight(path)
  if (status === null) setRow(index, 'fail', tr('checkRouteError', state.lang))
  else if (status >= 200 && status < 300) setRow(index, 'pass', tf('checkRoutePass', state.lang, status))
  else setRow(index, 'fail', tf('checkRouteFail', state.lang, status))
  render()
}

async function runChecks(): Promise<void> {
  if (running) return
  running = true
  finishedAt = null
  summaryKey = 'checksSummaryRunning'
  summaryArgs = [CHECKS.length]
  for (const row of rows) {
    row.state = 'pending'
    row.detail = null
  }
  render()

  const steps: (() => Promise<void>)[] = [
    checkService,
    checkAccounts,
    checkTokens,
    () => checkRoute(3, '/v1/responses'),
    () => checkRoute(4, '/v1/chat/completions'),
    () => checkRoute(5, '/v1/messages'),
  ]
  for (const step of steps) {
    try {
      await step()
    } catch (err) {
      console.error('[console] check failed', err)
    }
  }
  running = false
  finishedAt = Date.now()
  const passed = rows.filter((row) => row.state === 'pass').length
  if (passed === rows.length) {
    summaryKey = 'checksSummaryDone'
    summaryArgs = [rows.length]
  } else {
    summaryKey = 'checksSummaryPartial'
    summaryArgs = [passed, rows.length]
  }
  render()
}

function iconFor(mode: CheckState): SVGSVGElement {
  if (mode === 'pass') return icon('check')
  if (mode === 'fail') return icon('cross')
  if (mode === 'running') return icon('arrow')
  return icon('dot')
}

function stateLabel(mode: CheckState): string {
  const lang = state.lang
  if (mode === 'pass') return tr('checkPassed', lang)
  if (mode === 'fail') return tr('checkFailed', lang)
  if (mode === 'running') return tr('checkRunning', lang)
  return tr('checkPending', lang)
}

function render(): void {
  const node = host()
  if (!node) return
  const lang = state.lang
  clear(node)
  const summary = tf(summaryKey, lang, ...summaryArgs)
  const when = finishedAt === null ? null : dateTime(Math.floor(finishedAt / 1000), lang)
  const list = h('ul', { class: 'check-list' })
  rows.forEach((row) => {
    list.append(
      h(
        'li',
        { class: 'check-row', 'data-state': row.state },
        h('span', { class: 'check-icon', 'aria-hidden': 'true' }, iconFor(row.state)),
        h(
          'div',
          {},
          h('p', { class: 'check-name' }, tr(row.key, lang)),
          row.detail ? h('p', { class: 'check-detail' }, row.detail) : null,
        ),
        h('span', { class: 'check-state' }, stateLabel(row.state)),
      ),
    )
  })
  node.append(
    pageHead('health-title', tr('diagnosticsTitle', lang), tr('diagnosticsLede', lang), [
      h(
        'button',
        {
          type: 'button',
          class: 'btn btn-primary',
          disabled: running,
          'aria-busy': String(running),
          onclick: () => void runChecks(),
        },
        running ? tr('runningChecks', lang) : tr('runChecks', lang),
      ),
    ]),
    h(
      'div',
      { class: 'check-summary', role: 'status' },
      h('strong', {}, summary),
      when ? h('span', { class: 'section-note' }, tf('checksFinishedAt', lang, when)) : null,
    ),
    list,
  )
}

let attached = false

export const attachHealthPage = {
  render,
  attach(): void {
    ensureCatalogue()
    if (attached) return
    attached = true
    void reloadAccounts()
    void reloadStatus()
    void runChecks()
  },
}
