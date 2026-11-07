// One mutable view model shared by the pages. Every mutation goes through
// update(), which re-renders the current page inside its own isolation guard.

import { ApiError } from './api.ts'
import type { Failure } from './api.ts'
import { failureFrom } from './api.ts'
import {
  fetchAccounts,
  fetchStatus,
  loadCatalogue,
  refreshBalances,
  refreshCatalogue,
  runProbe,
  saveOrbLink,
  startLogin,
  switchAccount,
} from './data.ts'
import type { AccountRow, CatalogueRefresh, ModelRow, ProbeReport, StatusRow } from './data.ts'
import { state, saveProbeModel } from './state.ts'

type Phase = 'idle' | 'loading' | 'ready' | 'error'

export interface CatalogueSlice {
  phase: Phase
  models: ModelRow[]
  error: Failure | null
}

export interface AccountsSlice {
  phase: Phase
  rows: AccountRow[]
  current: string | null
  error: Failure | null
  loadedAt: number | null
}

export interface StatusSlice {
  phase: Phase
  checkedAt: number | null
  rows: Record<string, StatusRow>
  error: Failure | null
  /** True when rows are kept from an earlier successful load. */
  stale: boolean
}

export interface BusyState {
  status: boolean
  balance: boolean
  probeAll: boolean
  catalogue: boolean
  probe: Record<string, boolean>
  switch: Record<string, boolean>
  orb: Record<string, boolean>
}

export interface ProbeInfo {
  report: ProbeReport
  checkedAt: number | null
  /** Model that produced the report; differs from state.probeModel after a change. */
  model: string
}

export interface OverviewSlice {
  alive: boolean | null
  modelCount: number | null
}

export interface View {
  catalogue: CatalogueSlice
  accounts: AccountsSlice
  status: StatusSlice
  overview: OverviewSlice
  busy: BusyState
  probe: ProbeInfo | null
}

export const view: View = {
  catalogue: { phase: 'idle', models: [], error: null },
  accounts: { phase: 'idle', rows: [], current: null, error: null, loadedAt: null },
  status: { phase: 'idle', checkedAt: null, rows: {}, error: null, stale: false },
  overview: { alive: null, modelCount: null },
  busy: { status: false, balance: false, probeAll: false, catalogue: false, probe: {}, switch: {}, orb: {} },
  probe: null,
}

export function setModelCount(count: number): void {
  update(() => {
    view.overview.modelCount = count
  })
}

const listeners = new Set<() => void>()

export function onChange(listener: () => void): void {
  listeners.add(listener)
}

export function update(mutate: () => void): void {
  try {
    mutate()
  } catch (err) {
    console.error('[console] state update failed', err)
  }
  for (const listener of listeners) {
    try {
      listener()
    } catch (err) {
      console.error('[console] store listener failed', err)
    }
  }
}

export function ensureCatalogue(): void {
  if (view.catalogue.phase === 'loading') return
  if (view.catalogue.phase === 'ready' || view.catalogue.phase === 'error') return
  void reloadCatalogue()
}

export async function reloadCatalogue(): Promise<void> {
  update(() => {
    view.catalogue.phase = 'loading'
    view.catalogue.error = null
  })
  try {
    const catalogue = await loadCatalogue()
    update(() => {
      view.catalogue.phase = 'ready'
      view.catalogue.models = catalogue.models
      view.overview.modelCount = catalogue.models.length
    })
    reconcileProbeModel()
  } catch (err) {
    console.error('[console] catalogue load failed', err)
    update(() => {
      view.catalogue.phase = 'error'
      view.catalogue.error = failureFrom(err)
    })
  }
}

/** Falls back to the default or the first entry and writes the result back. */
export function reconcileProbeModel(): void {
  const models = view.catalogue.models
  if (models.length === 0) return
  if (models.some((row) => row.slug === state.probeModel)) return
  const fallback =
    models.find((row) => row.slug === 'gpt-5.6-luna') ?? models[0] ?? null
  if (fallback) saveProbeModel(fallback.slug)
}

export function selectProbeModel(slug: string): void {
  if (slug === state.probeModel) return
  saveProbeModel(slug)
  update(() => {})
}

export function modelBySlug(slug: string): ModelRow | null {
  return view.catalogue.models.find((row) => row.slug === slug) ?? null
}

export async function reloadAccounts(): Promise<void> {
  if (view.accounts.phase === 'loading') return
  update(() => {
    view.accounts.phase = 'loading'
    view.accounts.error = null
  })
  try {
    const result = await fetchAccounts()
    update(() => {
      view.accounts.phase = 'ready'
      view.accounts.rows = result.accounts
      view.accounts.current = result.current
      view.accounts.loadedAt = Date.now()
    })
  } catch (err) {
    console.error('[console] account list failed', err)
    update(() => {
      view.accounts.phase = 'error'
      view.accounts.error = failureFrom(err)
    })
  }
}

export async function reloadStatus(): Promise<void> {
  if (view.busy.status) return
  update(() => {
    view.busy.status = true
    view.status.phase = 'loading'
    view.status.error = null
  })
  try {
    const result = await fetchStatus()
    const rows: Record<string, StatusRow> = {}
    for (const row of result.rows) rows[row.name] = row
    update(() => {
      view.status.phase = 'ready'
      view.status.checkedAt = result.checkedAt
      view.status.rows = rows
      view.status.stale = false
    })
  } catch (err) {
    console.error('[console] account status failed', err)
    update(() => {
      view.busy.status = false
      view.status.phase = 'error'
      view.status.error = failureFrom(err)
      view.status.stale = Object.keys(view.status.rows).length > 0
    })
  } finally {
    update(() => {
      view.busy.status = false
    })
  }
}

export interface ProbeOutcome {
  passed: number
  total: number
  report: ProbeReport
  model: string
}

export async function probeAccount(account: string | null): Promise<ProbeOutcome | Failure> {
  const model = state.probeModel
  if (account === null) {
    if (view.busy.probeAll) return failureFrom(new Error('busy'))
    update(() => {
      view.busy.probeAll = true
    })
  } else {
    if (view.busy.probe[account]) return failureFrom(new Error('busy'))
    update(() => {
      view.busy.probe[account] = true
    })
  }
  try {
    const result = await runProbe(account, model)
    let passed = 0
    for (const row of result.rows) {
      if (row.modelState === 'healthy') passed += 1
      const existing = view.status.rows[row.name]
      view.status.rows[row.name] = existing
        ? { ...existing, ...row }
        : row
    }
    if (result.checkedAt !== null) view.status.checkedAt = result.checkedAt
    update(() => {
      view.probe = { report: result.report, checkedAt: result.checkedAt, model }
    })
    return { passed, total: result.rows.length, report: result.report, model }
  } catch (err) {
    console.error('[console] probe failed', err)
    return failureFrom(err)
  } finally {
    update(() => {
      if (account === null) view.busy.probeAll = false
      else delete view.busy.probe[account]
    })
  }
}

export async function switchTo(account: string): Promise<Failure | null> {
  if (view.busy.switch[account]) return null
  update(() => {
    view.busy.switch[account] = true
  })
  try {
    await switchAccount(account)
    await reloadAccounts()
    return null
  } catch (err) {
    console.error('[console] switch failed', err)
    return failureFrom(err)
  } finally {
    update(() => {
      delete view.busy.switch[account]
    })
  }
}

export async function storeOrbLink(account: string, url: string | null): Promise<Failure | null> {
  if (view.busy.orb[account]) return null
  update(() => {
    view.busy.orb[account] = true
  })
  try {
    await saveOrbLink(account, url)
    return null
  } catch (err) {
    console.error('[console] orb link save failed', err)
    return failureFrom(err)
  } finally {
    update(() => {
      delete view.busy.orb[account]
    })
  }
}

export interface BalanceOutcome {
  updated: number
  failed: number
  retryAfterS: number
  firstError: string | null
}

export async function refreshAllBalances(): Promise<BalanceOutcome | Failure> {
  if (view.busy.balance) return failureFrom(new Error('busy'))
  update(() => {
    view.busy.balance = true
  })
  try {
    const result = await refreshBalances()
    let updated = 0
    let failed = 0
    let firstError: string | null = null
    for (const entry of result.entries) {
      const credit = entry.credit
      if (!credit) continue
      if (credit.ok) {
        updated += 1
        const existing = view.status.rows[entry.account]
        if (existing) view.status.rows[entry.account] = { ...existing, credit, orbConfigured: true }
      } else {
        failed += 1
        if (firstError === null) firstError = credit.error
        const existing = view.status.rows[entry.account]
        // Keep the previous successful numbers; just record that the read failed.
        if (existing) view.status.rows[entry.account] = { ...existing, orbConfigured: true }
      }
    }
    update(() => {})
    return { updated, failed, retryAfterS: result.retryAfterS, firstError }
  } catch (err) {
    console.error('[console] balance refresh failed', err)
    return failureFrom(err)
  } finally {
    update(() => {
      view.busy.balance = false
    })
  }
}

export async function refreshCatalogueNow(): Promise<CatalogueRefresh | Failure> {
  if (view.busy.catalogue) return failureFrom(new Error('busy'))
  update(() => {
    view.busy.catalogue = true
  })
  try {
    const result = await refreshCatalogue()
    if (result.refreshed) {
      await loadCatalogue(true)
      update(() => {
        view.catalogue.phase = 'idle'
      })
      await reloadCatalogue()
    }
    return result
  } catch (err) {
    console.error('[console] catalogue refresh failed', err)
    return failureFrom(err)
  } finally {
    update(() => {
      view.busy.catalogue = false
    })
  }
}

export async function beginLogin(): Promise<{ url: string | null; expiresIn: number | null } | Failure> {
  try {
    const start = await startLogin()
    return { url: start.loginUrl, expiresIn: start.expiresIn }
  } catch (err) {
    console.error('[console] login start failed', err)
    return failureFrom(err)
  }
}

export function isFailure(value: unknown): value is Failure {
  return typeof value === 'object' && value !== null && 'kind' in value && 'status' in value
}

export function apiStatus(err: unknown): number | null {
  return err instanceof ApiError ? err.status : null
}
