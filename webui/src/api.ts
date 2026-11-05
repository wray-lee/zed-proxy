import { t } from './i18n'

export interface Account {
  name: string
  user_id: string
  current: boolean
  healthy?: boolean
  cooldown_s?: number
  consecutive_failures?: number
  last_status?: number
}

export interface AccountsResponse {
  accounts: Account[]
  current: string
}

export interface AccountCredit {
  ok: boolean
  balance_usd?: number
  cap_usd?: number
  used_usd?: number
  used_pct?: number
  period_start?: string
  resets_at?: string
  total_balance_usd?: number
  error?: string
}

export interface AccountQuotaStatus {
  name: string
  user_id: string
  current: boolean
  scheduler_healthy: boolean
  cooldown_s: number
  last_status: number
  check_ok: boolean
  token_ok: boolean
  billing_ok: boolean
  usable: boolean
  quota_state: 'available' | 'exhausted' | 'unmetered' | 'restricted' | 'unknown' | 'unavailable'
  plan?: string
  used?: number | null
  limit?: number | null
  remaining?: number | null
  subscription_ends_at?: string | null
  usage_based_billing?: boolean
  overdue?: boolean
  account_too_young?: boolean
  model_ok?: boolean
  model_state?: AccountModelState
  model_checked_at?: number
  model_latency_ms?: number
  error?: string
  orb_configured?: boolean
  credit?: AccountCredit
}

export interface AccountStatusesResponse {
  checked_at: number
  accounts: AccountQuotaStatus[]
}

export type AccountModelState = 'unchecked' | 'healthy' | 'auth_error' | 'rate_limited' | 'upstream_error'

export interface AccountHealthResult {
  name: string
  model_ok: boolean
  model_state: AccountModelState
  model_checked_at: number
  model_latency_ms: number
  scheduler_healthy: boolean
  cooldown_s: number
  last_status: number
}

export interface AccountHealthResponse {
  checked_at: number
  probe: {
    model: string
    // null when the selected model/provider has no effort knob.
    reasoning_effort: string | null
    // null when the provider request carries no output cap (Gemini).
    max_output_tokens: number | null
  }
  accounts: AccountHealthResult[]
}

export interface UsageInfo {
  plan?: string
  monthly_spend_in_cents?: number
  monthly_spending_limit_in_cents?: number
  subscriptionPeriod?: string[]
  githubUserLogin?: string
  // from /client/users/me
  user?: { github_login?: string; name?: string; avatar_url?: string }
  planInfo?: {
    plan?: string
    subscription_period?: { started_at?: string; ended_at?: string }
    usage?: { model_requests?: { used?: number; limit?: { limited?: number } } }
  }
  [key: string]: unknown
}

const API_KEY_STORAGE = 'zed_api_key'

/** 站点数据被禁用时只有内存里的 key 可用，至少本次会话还能工作。 */
let sessionApiKey = ''

function storedApiKey(): string {
  try {
    return localStorage.getItem(API_KEY_STORAGE) ?? sessionApiKey
  } catch {
    return sessionApiKey
  }
}

function withApiKey(init?: RequestInit): RequestInit {
  const headers = new Headers(init?.headers)
  const key = storedApiKey()
  if (key) headers.set('Authorization', `Bearer ${key}`)
  return { ...init, headers }
}

/**
 * fetch with the gateway API key from localStorage. On 401 it asks for the
 * key, stores it and retries once; cancelling the prompt returns the original
 * 401 response so callers keep their existing error handling.
 */
export async function authFetch(input: RequestInfo | URL, init?: RequestInit): Promise<Response> {
  const first = await globalThis.fetch(input, withApiKey(init))
  if (first.status !== 401) return first
  const entered = prompt(t('请输入网关 API Key', 'Enter the gateway API key'))
  if (entered == null || entered.trim() === '') return first
  sessionApiKey = entered.trim()
  try {
    localStorage.setItem(API_KEY_STORAGE, sessionApiKey)
  } catch {
    // 存储被禁用：仅本次会话有效。
  }
  return globalThis.fetch(input, withApiKey(init))
}

export async function fetchAccounts(): Promise<AccountsResponse> {
  const r = await authFetch('/zed/accounts')
  if (!r.ok) throw new Error(`${r.status}`)
  return r.json()
}

export interface ModelInfo {
  slug: string
  display_name: string
  description?: string
  default_reasoning_level?: string
  supported_reasoning_levels?: { effort: string; description?: string }[]
}

export interface ModelsResponse {
  data?: { id: string; owned_by?: string }[]
  models?: ModelInfo[]
}

export async function fetchModels(): Promise<ModelsResponse> {
  const r = await authFetch('/v1/models', { cache: 'no-store' })
  if (!r.ok) throw new Error(`${r.status}`)
  return r.json()
}

export async function fetchAccountStatuses(): Promise<AccountStatusesResponse> {
  const r = await authFetch('/zed/accounts/status')
  if (!r.ok) throw new Error(`${r.status}`)
  return r.json()
}

export interface OrbRefreshEntry {
  account: string
  refreshed: boolean
  retry_after_s: number
  credit: AccountCredit
}

export interface OrbRefreshResponse {
  refreshed: boolean
  // 单账号请求带这三个；全部账号请求带 accounts。
  account?: string
  retry_after_s?: number
  credit?: AccountCredit
  accounts?: OrbRefreshEntry[]
}

/** 绕过 300/60 秒缓存重新拉取 Orb 余额；省略 account 表示全部已关联账号。 */
export async function refreshOrbCredit(account?: string): Promise<OrbRefreshResponse> {
  const r = await authFetch('/zed/accounts/orb/refresh', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(account ? { account } : {}),
  })
  if (!r.ok) throw new Error(await errorCode(r))
  return r.json()
}

export interface ModelsRefreshResponse {
  refreshed: boolean
  count?: number
  retry_after_s?: number
}

/** 强制重新拉取上游模型目录；502 表示上游失败，调用方保留旧列表。 */
export async function refreshModels(): Promise<ModelsRefreshResponse> {
  const r = await authFetch('/zed/models/refresh', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({}),
  })
  if (!r.ok) throw new Error(await errorCode(r))
  return r.json()
}

/** 错误体里的 error 字段比状态码更能说明原因；没有就退回状态码。 */
async function errorCode(r: Response): Promise<string> {
  try {
    const data = await r.json()
    if (typeof data?.error === 'string' && data.error) return data.error
  } catch {
    // 非 JSON 错误体只报状态码。
  }
  return `${r.status}`
}

/**
 * Run an explicit inference probe. Omitting `name` checks all accounts;
 * callers should only invoke this from a user action because it consumes one
 * deliberately tiny model request per selected account.
 */
export async function checkAccountHealth(name?: string, model?: string): Promise<AccountHealthResponse> {
  const body: { account?: string; model?: string } = {}
  if (name) body.account = name
  if (model) body.model = model
  const r = await authFetch('/zed/accounts/health', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(body),
  })
  if (!r.ok) {
    const detail = await r.text()
    throw new Error(`${r.status}${detail ? `: ${detail}` : ''}`)
  }
  return r.json()
}

/** Store or clear the Orb portal link for an account; the link is never returned. */
export async function setAccountOrb(account: string, url: string): Promise<void> {
  const r = await authFetch('/zed/accounts/orb', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ account, url }),
  })
  if (r.ok) return
  let code = `${r.status}`
  try {
    const data = await r.json()
    if (data?.error) code = data.error
  } catch {
    // 非 JSON 错误体只报状态码。
  }
  throw new Error(code)
}

export async function switchAccount(name: string): Promise<void> {
  const r = await authFetch('/zed/accounts/switch', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ account: name }),
  })
  if (!r.ok) throw new Error(`${r.status}`)
}

export async function fetchUsage(): Promise<UsageInfo> {
  const r = await authFetch('/zed/usage')
  if (!r.ok) throw new Error(`${r.status}`)
  return r.json()
}

export async function fetchBilling(): Promise<Record<string, unknown>> {
  const r = await authFetch('/zed/billing')
  if (!r.ok) throw new Error(`${r.status}`)
  return r.json()
}

export interface LoginStartResponse {
  login_url?: string
  port?: number
  mode?: string
  expires_in?: number
  error?: string
}

export async function startLogin(name?: string, mode?: 'paste'): Promise<LoginStartResponse> {
  const r = await authFetch('/zed/login', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ name, mode }),
  })
  return r.json()
}

export async function completeLogin(url: string): Promise<{ status: string; name: string }> {
  const r = await authFetch('/zed/login/complete', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ url }),
  })
  if (!r.ok) {
    let message = `${r.status}`
    try {
      const data = await r.json()
      if (data?.error) message = data.detail ? t(`${data.error}：${data.detail}`, `${data.error}: ${data.detail}`) : data.error
    } catch {
      // 非 JSON 错误体（如网关 502）只报状态码。
    }
    throw new Error(message)
  }
  return r.json()
}

export interface ChatMessage {
  role: 'user' | 'assistant' | 'system'
  content: string
}

export async function sendOpenAI(
  model: string,
  messages: ChatMessage[],
  maxTokens = 4096,
): Promise<string> {
  const r = await authFetch('/v1/chat/completions', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ model, messages, max_tokens: maxTokens }),
  })
  const d = await r.json()
  return d.choices?.[0]?.message?.content ?? JSON.stringify(d, null, 2)
}

export async function sendAnthropic(
  model: string,
  messages: ChatMessage[],
  maxTokens = 4096,
): Promise<string> {
  const r = await authFetch('/v1/messages', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ model, messages, max_tokens: maxTokens }),
  })
  const d = await r.json()
  return d.content?.[0]?.text ?? JSON.stringify(d, null, 2)
}
