import { authFetch } from '../api'
import { icons } from '../icons'
import { locale, t } from '../i18n'

interface CheckResult {
  name: string
  description: string
  status: 'ok' | 'fail' | 'pending'
  detail: string
  latency?: number
}

interface CheckDefinition {
  name: string
  description: string
  run: () => Promise<Omit<CheckResult, 'name' | 'description'>>
}

const CHECKS: CheckDefinition[] = [
  {
    name: t('API 服务', 'API service'),
    description: t('确认本地网关能够返回模型列表', 'Confirm the local gateway can return the model list'),
    run: async () => {
      const started = performance.now()
      const response = await authFetch('/v1/models', { cache: 'no-store' })
      const latency = Math.round(performance.now() - started)
      if (!response.ok) return { status: 'fail', detail: `HTTP ${response.status}`, latency }
      const data = await response.json() as { data?: unknown[] }
      const count = data.data?.length ?? 0
      return { status: 'ok', detail: t(`已加载 ${count} 个模型`, `Loaded ${count} ${count === 1 ? 'model' : 'models'}`), latency }
    },
  },
  {
    name: t('账号配置', 'Account setup'),
    description: t('确认至少存在一个可调度账号', 'Confirm at least one schedulable account exists'),
    run: async () => {
      const started = performance.now()
      const response = await authFetch('/zed/accounts', { cache: 'no-store' })
      const latency = Math.round(performance.now() - started)
      if (!response.ok) return { status: 'fail', detail: `HTTP ${response.status}`, latency }
      const data = await response.json() as { accounts?: { name: string; current: boolean }[] }
      const accountList = data.accounts ?? []
      if (accountList.length === 0) return { status: 'fail', detail: t('尚未配置账号', 'No accounts configured yet'), latency }
      const current = accountList.find(account => account.current)
      const count = accountList.length
      const currentName = current?.name ?? t('未选择', 'none selected')
      return { status: 'ok', detail: t(`${count} 个账号，当前：${currentName}`, `${count} ${count === 1 ? 'account' : 'accounts'}, current: ${currentName}`), latency }
    },
  },
  {
    name: t('令牌与额度', 'Tokens and quota'),
    description: t('被动检查全部账号，不产生模型请求', 'Passively check all accounts without sending model requests'),
    run: async () => {
      const started = performance.now()
      const response = await authFetch('/zed/accounts/status', { cache: 'no-store' })
      const latency = Math.round(performance.now() - started)
      if (!response.ok) return { status: 'fail', detail: `HTTP ${response.status}`, latency }
      const data = await response.json() as { accounts?: { token_ok: boolean }[] }
      const accountList = data.accounts ?? []
      const usable = accountList.filter(account => account.token_ok).length
      const total = accountList.length
      return {
        status: usable === total && total > 0 ? 'ok' : 'fail',
        detail: t(`${usable}/${total} 个令牌可用`, `${usable}/${total} ${total === 1 ? 'token' : 'tokens'} available`),
        latency,
      }
    },
  },
  {
    name: t('Codex Responses 路由', 'Codex Responses route'),
    description: t('检查 /v1/responses 路由与跨域响应', 'Check the /v1/responses route and its CORS response'),
    run: async () => checkRoute('/v1/responses'),
  },
  {
    name: t('Chat Completions 路由', 'Chat Completions route'),
    description: t('检查 /v1/chat/completions 兼容路由', 'Check the /v1/chat/completions compatibility route'),
    run: async () => checkRoute('/v1/chat/completions'),
  },
  {
    name: t('Claude Messages 路由', 'Claude Messages route'),
    description: t('检查 /v1/messages 原生协议路由', 'Check the /v1/messages native protocol route'),
    run: async () => checkRoute('/v1/messages'),
  },
]

async function checkRoute(path: string): Promise<Omit<CheckResult, 'name' | 'description'>> {
  const started = performance.now()
  try {
    const response = await fetch(path, { method: 'OPTIONS' })
    const latency = Math.round(performance.now() - started)
    return { status: response.ok ? 'ok' : 'fail', detail: `HTTP ${response.status}`, latency }
  } catch (error) {
    return { status: 'fail', detail: error instanceof Error ? error.message : t('路由不可达', 'Route unreachable') }
  }
}

function escapeHtml(value: string): string {
  const element = document.createElement('div')
  element.textContent = value
  return element.innerHTML
}

export function renderHealth() {
  const page = document.getElementById('page-health')!
  page.innerHTML = `
    <div class="page-heading">
      <div>
        <span class="eyebrow">${t('只读诊断', 'Read-only diagnostics')}</span>
        <h1>${t('服务检查', 'Health check')}</h1>
        <p>${t('检查服务、账号、令牌和三种协议路由；本页面不会主动发送模型推理请求。', 'Checks the service, accounts, tokens and all three protocol routes; this page never sends model inference requests.')}</p>
      </div>
      <div class="heading-actions">
        <button class="button secondary" id="rerun-button" type="button">${icons.refresh} ${t('重新检查', 'Run checks again')}</button>
      </div>
    </div>

    <div class="notice-banner neutral">
      <span>${icons.alertCircle}</span>
      <p>${t('路由检查只验证接口是否可达。需要验证真实模型输出时，请前往“账号中心”执行模型实测。', 'Route checks only verify that endpoints are reachable. To verify real model output, run a live model check in Account center.')}</p>
    </div>

    <div class="health-summary" id="health-summary"></div>
    <div class="health-list" id="health-list"></div>
  `

  document.getElementById('rerun-button')!.addEventListener('click', () => void runChecks())
  void runChecks()
}

async function runChecks() {
  const list = document.getElementById('health-list')!
  const summary = document.getElementById('health-summary')!
  const rerunButton = document.getElementById('rerun-button') as HTMLButtonElement
  rerunButton.disabled = true
  rerunButton.innerHTML = `<span class="spinner"></span>${t('正在检查', 'Checking')}`

  list.innerHTML = CHECKS.map(check => `
    <article class="health-row pending">
      <span class="health-icon"><span class="spinner"></span></span>
      <div class="health-copy">
        <strong>${check.name}</strong>
        <p>${check.description}</p>
      </div>
      <span class="health-result">${t('检查中', 'Checking')}</span>
    </article>
  `).join('')
  summary.className = 'health-summary checking'
  summary.innerHTML = `<span class="spinner"></span><div><strong>${t(`正在执行 ${CHECKS.length} 项检查`, `Running ${CHECKS.length} checks`)}</strong><p>${t('请稍候，检查完成后会显示每项耗时。', 'Please wait; per-check latency appears when the run finishes.')}</p></div>`

  const results: CheckResult[] = []
  for (let index = 0; index < CHECKS.length; index += 1) {
    const check = CHECKS[index]
    let result: CheckResult
    try {
      result = { name: check.name, description: check.description, ...await check.run() }
    } catch (error) {
      result = {
        name: check.name,
        description: check.description,
        status: 'fail',
        detail: error instanceof Error ? error.message : t('检查异常', 'Check failed'),
      }
    }
    results.push(result)

    const row = list.querySelectorAll<HTMLElement>('.health-row')[index]
    if (row) {
      row.className = `health-row ${result.status}`
      row.innerHTML = `
        <span class="health-icon">${result.status === 'ok' ? icons.checkCircle : icons.xCircle}</span>
        <div class="health-copy"><strong>${result.name}</strong><p>${result.description}</p></div>
        <div class="health-result"><strong>${escapeHtml(result.detail)}</strong>${result.latency != null ? `<span>${t(`${result.latency} 毫秒`, `${result.latency} ms`)}</span>` : ''}</div>
      `
    }
  }

  const passed = results.filter(result => result.status === 'ok').length
  const allPassed = passed === results.length
  summary.className = `health-summary ${allPassed ? 'ok' : 'warn'}`
  summary.innerHTML = `
    <span class="health-summary-icon">${allPassed ? icons.checkCircle : icons.alertCircle}</span>
    <div>
      <strong>${allPassed ? t('全部基础检查通过', 'All basic checks passed') : t(`${passed}/${results.length} 项检查通过`, `${passed}/${results.length} ${results.length === 1 ? 'check' : 'checks'} passed`)}</strong>
      <p>${t('完成时间：', 'Completed at: ')}${new Date().toLocaleString(locale, { hour12: false })}</p>
    </div>
  `
  rerunButton.disabled = false
  rerunButton.innerHTML = `${icons.refresh} ${t('重新检查', 'Run checks again')}`
}
