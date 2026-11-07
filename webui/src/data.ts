// Typed views over the gateway's JSON plus the shared catalogue cache.
// Parsing is defensive: a missing field becomes null, never an exception.

import { request, requestPublic } from './api.ts'
import { arr, bool, isRecord, num, str } from './dom.ts'

export interface ModelRow {
  slug: string
  displayName: string
  description: string | null
  ownedBy: string | null
  defaultEffort: string | null
  levels: string[]
  contextLength: number | null
  maxOutputTokens: number | null
  /** True for a row that came only from `data[]`, with no catalogue entry. */
  upstreamOnly: boolean
}

export interface Catalogue {
  models: ModelRow[]
}

export interface AccountRow {
  name: string
  userId: string | null
  current: boolean
  healthy: boolean
  cooldownS: number
  lastStatus: number
}

export interface AccountsView {
  accounts: AccountRow[]
  current: string | null
}

export interface StatusRow {
  name: string
  checkedAt: number | null
  quotaState: string | null
  tokenOk: boolean | null
  checkOk: boolean | null
  billingOk: boolean | null
  plan: string | null
  used: number | null
  limit: number | null
  remaining: number | null
  usageBased: boolean | null
  /** Billing flags; null means the response did not carry the flag. */
  overdue: boolean | null
  accountTooYoung: boolean | null
  subscriptionEndsAt: string | null
  orbConfigured: boolean | null
  credit: Credit | null
  modelState: string | null
  modelCheckedAt: number | null
  modelLatencyMs: number | null
  cooldownS: number | null
}

/** The fields one response entry actually carried; absent ones are omitted. */
export type StatusPatch = { name: string } & Partial<Omit<StatusRow, 'name'>>

/** Overlays only the fields the patch carried, so a probe keeps credit and flags. */
export function applyStatusPatch(existing: StatusRow | undefined, patch: StatusPatch): StatusRow {
  return existing ? { ...existing, ...patch } : { ...emptyStatusRow(patch.name), ...patch }
}

export interface Credit {
  ok: boolean
  balanceUsd: number | null
  capUsd: number | null
  usedUsd: number | null
  usedPct: number | null
  resetsAt: string | null
  error: string | null
}

export interface StatusView {
  checkedAt: number | null
  rows: StatusRow[]
}

export interface ProbeReport {
  model: string | null
  effort: string | null
  maxOutputTokens: number | null
}

export interface ProbeResult {
  report: ProbeReport
  patches: StatusPatch[]
  checkedAt: number | null
}

export interface LoginStart {
  loginUrl: string | null
  expiresIn: number | null
  error: string | null
}

export interface CreditEntry {
  account: string
  refreshed: boolean
  retryAfterS: number
  credit: Credit | null
}

export interface BalanceRefresh {
  refreshed: boolean
  retryAfterS: number
  entries: CreditEntry[]
}

export interface CatalogueRefresh {
  refreshed: boolean
  retryAfterS: number
  count: number | null
}

// ---- Parsers ----------------------------------------------------------------

function parseModel(entry: unknown, ownedBy: string | null, contextLength: number | null, maxOut: number | null): ModelRow | null {
  if (!isRecord(entry)) return null
  const slug = str(entry.slug)
  if (!slug) return null
  const levels = arr(entry.supported_reasoning_levels)
    .map((lvl) => (isRecord(lvl) ? str(lvl.effort) : null))
    .filter((v): v is string => v !== null)
  return {
    slug,
    displayName: str(entry.display_name) ?? slug,
    description: str(entry.description),
    ownedBy,
    defaultEffort: str(entry.default_reasoning_level),
    levels,
    contextLength,
    maxOutputTokens: maxOut,
    upstreamOnly: false,
  }
}

export function parseCatalogue(payload: unknown): Catalogue {
  const record = isRecord(payload) ? payload : {}
  const dataById = new Map<string, { ownedBy: string | null; context: number | null; maxOut: number | null }>()
  for (const item of arr(record.data)) {
    if (!isRecord(item)) continue
    const id = str(item.id)
    if (!id) continue
    dataById.set(id, { ownedBy: str(item.owned_by), context: num(item.context_length), maxOut: num(item.max_output_tokens) })
  }
  const models: ModelRow[] = []
  const seen = new Set<string>()
  for (const entry of arr(record.models)) {
    if (!isRecord(entry)) continue
    const slug = str(entry.slug)
    if (!slug || seen.has(slug)) continue
    const extra = dataById.get(slug)
    const row = parseModel(entry, extra?.ownedBy ?? null, extra?.context ?? null, extra?.maxOut ?? null)
    if (!row) continue
    seen.add(slug)
    models.push(row)
  }
  for (const [id, extra] of dataById) {
    if (seen.has(id)) continue
    seen.add(id)
    models.push({
      slug: id,
      displayName: id,
      description: null,
      ownedBy: extra.ownedBy,
      defaultEffort: null,
      levels: [],
      contextLength: extra.context,
      maxOutputTokens: extra.maxOut,
      upstreamOnly: true,
    })
  }
  return { models }
}

export function parseAccounts(payload: unknown): AccountsView {
  const record = isRecord(payload) ? payload : {}
  const accounts: AccountRow[] = []
  for (const entry of arr(record.accounts)) {
    if (!isRecord(entry)) continue
    const name = str(entry.name)
    if (!name) continue
    accounts.push({
      name,
      userId: str(entry.user_id),
      current: bool(entry.current),
      healthy: bool(entry.healthy),
      cooldownS: num(entry.cooldown_s) ?? 0,
      lastStatus: num(entry.last_status) ?? 0,
    })
  }
  const current = str(record.current)
  return { accounts, current: current === '' ? null : current }
}

function parseCredit(raw: unknown): Credit | null {
  if (!isRecord(raw)) return null
  const ok = bool(raw.ok)
  return {
    ok,
    balanceUsd: num(raw.balance_usd),
    capUsd: num(raw.cap_usd),
    usedUsd: num(raw.used_usd),
    usedPct: num(raw.used_pct),
    resetsAt: str(raw.resets_at),
    error: ok ? null : str(raw.error),
  }
}

export function emptyStatusRow(name: string): StatusRow {
  return {
    name,
    checkedAt: null,
    quotaState: null,
    tokenOk: null,
    checkOk: null,
    billingOk: null,
    plan: null,
    used: null,
    limit: null,
    remaining: null,
    usageBased: null,
    overdue: null,
    accountTooYoung: null,
    subscriptionEndsAt: null,
    orbConfigured: null,
    credit: null,
    modelState: null,
    modelCheckedAt: null,
    modelLatencyMs: null,
    cooldownS: null,
  }
}

function boolOrNull(value: unknown): boolean | null {
  return typeof value === 'boolean' ? value : null
}

/** Only the keys the entry carries; a probe response has far fewer than a status entry. */
function parseStatusPatch(entry: unknown): StatusPatch | null {
  if (!isRecord(entry)) return null
  const name = str(entry.name)
  if (!name) return null
  const patch: StatusPatch = { name }
  if ('checked_at' in entry) patch.checkedAt = num(entry.checked_at)
  if ('quota_state' in entry) patch.quotaState = str(entry.quota_state)
  if ('token_ok' in entry) patch.tokenOk = boolOrNull(entry.token_ok)
  if ('check_ok' in entry) patch.checkOk = boolOrNull(entry.check_ok)
  if ('billing_ok' in entry) patch.billingOk = boolOrNull(entry.billing_ok)
  if ('plan' in entry) patch.plan = str(entry.plan)
  if ('used' in entry) patch.used = num(entry.used)
  if ('limit' in entry) patch.limit = num(entry.limit)
  if ('remaining' in entry) patch.remaining = num(entry.remaining)
  if ('usage_based_billing' in entry) patch.usageBased = boolOrNull(entry.usage_based_billing)
  if ('overdue' in entry) patch.overdue = boolOrNull(entry.overdue)
  if ('account_too_young' in entry) patch.accountTooYoung = boolOrNull(entry.account_too_young)
  if ('subscription_ends_at' in entry) patch.subscriptionEndsAt = str(entry.subscription_ends_at)
  if ('orb_configured' in entry) patch.orbConfigured = boolOrNull(entry.orb_configured)
  if ('credit' in entry) patch.credit = parseCredit(entry.credit)
  if ('model_state' in entry) patch.modelState = str(entry.model_state)
  if ('model_checked_at' in entry) patch.modelCheckedAt = num(entry.model_checked_at)
  if ('model_latency_ms' in entry) patch.modelLatencyMs = num(entry.model_latency_ms)
  if ('cooldown_s' in entry) patch.cooldownS = num(entry.cooldown_s)
  return patch
}

export function parseStatus(payload: unknown): StatusView {
  const record = isRecord(payload) ? payload : {}
  const rows: StatusRow[] = []
  for (const entry of arr(record.accounts)) {
    const patch = parseStatusPatch(entry)
    if (patch) rows.push({ ...emptyStatusRow(patch.name), ...patch })
  }
  return { checkedAt: num(record.checked_at), rows }
}

export function parseProbe(payload: unknown): ProbeResult {
  const record = isRecord(payload) ? payload : {}
  const probe = isRecord(record.probe) ? record.probe : {}
  const patches: StatusPatch[] = []
  for (const entry of arr(record.accounts)) {
    const patch = parseStatusPatch(entry)
    if (patch) patches.push(patch)
  }
  return {
    report: {
      model: str(probe.model),
      effort: str(probe.reasoning_effort),
      maxOutputTokens: num(probe.max_output_tokens),
    },
    patches,
    checkedAt: num(record.checked_at),
  }
}

// ---- Endpoint wrappers ------------------------------------------------------

interface CacheEntry {
  value: Catalogue | null
  promise: Promise<Catalogue> | null
}

const catalogueCache: CacheEntry = { value: null, promise: null }

/** One catalogue fetch per page session; a failure is never cached. */
export function loadCatalogue(force = false): Promise<Catalogue> {
  if (force) {
    catalogueCache.value = null
    catalogueCache.promise = null
  }
  if (catalogueCache.value) return Promise.resolve(catalogueCache.value)
  if (catalogueCache.promise) return catalogueCache.promise
  const pending = request<unknown>('/v1/models', { method: 'GET' })
    .then((payload) => {
      const parsed = parseCatalogue(payload)
      catalogueCache.value = parsed
      return parsed
    })
    .finally(() => {
      catalogueCache.promise = null
    })
  catalogueCache.promise = pending
  return pending
}

export function cachedCatalogue(): Catalogue | null {
  return catalogueCache.value
}

export function peekCatalogueSync(): ModelRow[] {
  return catalogueCache.value?.models ?? []
}

export async function fetchAccounts(): Promise<AccountsView> {
  return parseAccounts(await request<unknown>('/zed/accounts'))
}

export async function fetchStatus(): Promise<StatusView> {
  return parseStatus(await request<unknown>('/zed/accounts/status'))
}

export async function runProbe(account: string | null, model: string): Promise<ProbeResult> {
  const body: Record<string, unknown> = { model }
  if (account !== null) body.account = account
  return parseProbe(await request<unknown>('/zed/accounts/health', { method: 'POST', body }))
}

export async function switchAccount(account: string): Promise<void> {
  await request<unknown>('/zed/accounts/switch', { method: 'POST', body: { account } })
}

export async function saveOrbLink(account: string, url: string | null): Promise<void> {
  await request<unknown>('/zed/accounts/orb', { method: 'POST', body: { account, url } })
}

export async function refreshBalances(): Promise<BalanceRefresh> {
  const payload = await request<unknown>('/zed/accounts/orb/refresh', { method: 'POST', body: {} })
  const record = isRecord(payload) ? payload : {}
  const entries: CreditEntry[] = []
  if (Array.isArray(record.accounts)) {
    for (const entry of record.accounts) {
      if (!isRecord(entry)) continue
      const account = str(entry.account)
      if (!account) continue
      entries.push({
        account,
        refreshed: bool(entry.refreshed),
        retryAfterS: num(entry.retry_after_s) ?? 0,
        credit: parseCredit(entry.credit),
      })
    }
  } else if (str(record.account)) {
    entries.push({
      account: str(record.account) as string,
      refreshed: bool(record.refreshed),
      retryAfterS: num(record.retry_after_s) ?? 0,
      credit: parseCredit(record.credit),
    })
  }
  let retry = num(record.retry_after_s) ?? 0
  for (const entry of entries) retry = Math.max(retry, entry.retryAfterS)
  return { refreshed: bool(record.refreshed), retryAfterS: retry, entries }
}

export async function refreshCatalogue(): Promise<CatalogueRefresh> {
  const payload = await request<unknown>('/zed/models/refresh', { method: 'POST', body: {} })
  const record = isRecord(payload) ? payload : {}
  return {
    refreshed: bool(record.refreshed),
    retryAfterS: num(record.retry_after_s) ?? 0,
    count: num(record.count),
  }
}

export async function startLogin(): Promise<LoginStart> {
  const payload = await request<unknown>('/zed/login', { method: 'POST', body: { mode: 'paste' } })
  const record = isRecord(payload) ? payload : {}
  return {
    loginUrl: str(record.login_url),
    expiresIn: num(record.expires_in),
    error: str(record.error),
  }
}

export async function completeLogin(url: string): Promise<string | null> {
  const payload = await request<unknown>('/zed/login/complete', { method: 'POST', body: { url } })
  const record = isRecord(payload) ? payload : {}
  return str(record.name)
}

export async function checkLiveness(): Promise<boolean> {
  try {
    await requestPublic<unknown>('/health')
    return true
  } catch {
    return false
  }
}

export async function preflight(path: string): Promise<number | null> {
  try {
    const response = await fetch(path, { method: 'OPTIONS', cache: 'no-store', credentials: 'omit' })
    return response.status
  } catch {
    return null
  }
}
