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
    reasoning_effort: string
    max_output_tokens: number
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

function withApiKey(init?: RequestInit): RequestInit {
  const headers = new Headers(init?.headers)
  const key = localStorage.getItem(API_KEY_STORAGE)
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
  localStorage.setItem(API_KEY_STORAGE, entered.trim())
  return globalThis.fetch(input, withApiKey(init))
}

export async function fetchAccounts(): Promise<AccountsResponse> {
  const r = await authFetch('/zed/accounts')
  if (!r.ok) throw new Error(`${r.status}`)
  return r.json()
}

export async function fetchAccountStatuses(): Promise<AccountStatusesResponse> {
  const r = await authFetch('/zed/accounts/status')
  if (!r.ok) throw new Error(`${r.status}`)
  return r.json()
}

/**
 * Run an explicit inference probe. Omitting `name` checks all accounts;
 * callers should only invoke this from a user action because it consumes one
 * deliberately tiny model request per selected account.
 */
export async function checkAccountHealth(name?: string): Promise<AccountHealthResponse> {
  const r = await authFetch('/zed/accounts/health', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(name ? { account: name } : {}),
  })
  if (!r.ok) {
    const detail = await r.text()
    throw new Error(`${r.status}${detail ? `: ${detail}` : ''}`)
  }
  return r.json()
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
