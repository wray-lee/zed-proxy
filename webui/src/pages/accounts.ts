import {
  checkAccountHealth,
  completeLogin,
  fetchAccounts,
  fetchAccountStatuses,
  refreshOrbCredit,
  setAccountOrb,
  startLogin,
  switchAccount,
  type Account,
  type AccountCredit,
  type AccountHealthResult,
  type AccountQuotaStatus,
  type ModelInfo,
} from '../api'
import { locale, t } from '../i18n'
import { icons } from '../icons'
import {
  catalogModels,
  getProbeModel,
  loadCatalog,
  onProbeModelChange,
  renderProbeModelOptions,
  setProbeModel,
} from '../probeModel'
import { showToast } from '../toast'

let accounts: Account[] = []
let statuses = new Map<string, AccountQuotaStatus>()
let models: ModelInfo[] = []
let lastCheckedAt: number | undefined
let refreshingAccountData = false
let checkingAllAccounts = false
let refreshingOrb = false
const checkingAccounts = new Set<string>()

function escapeHtml(value: string): string {
  // 账号元数据来自本地服务响应；统一按文本渲染，避免异常字段被浏览器当成 HTML。
  const element = document.createElement('div')
  element.textContent = value
  return element.innerHTML
}

function formatPlan(plan?: string): string {
  if (!plan || plan === 'unknown') return t('套餐未公开', 'Plan not disclosed')
  return plan.replace(/^zed_/, 'Zed ').replaceAll('_', ' ')
}

function formatDate(value?: string | null): string {
  if (!value) return t('未公开', 'Not disclosed')
  const date = new Date(value)
  if (Number.isNaN(date.getTime())) return t('未公开', 'Not disclosed')
  return date.toLocaleDateString(locale, { year: 'numeric', month: 'long', day: 'numeric' })
}

function formatCheckedAt(value?: number): string {
  if (!value) return t('尚未检测', 'Not checked yet')
  return new Date(value * 1000).toLocaleString(locale, { hour12: false })
}

function quotaPresentation(status: AccountQuotaStatus | undefined) {
  const unknown = t('未知', 'unknown')
  if (!status) {
    return {
      className: refreshingAccountData ? 'checking' : 'unchecked',
      label: refreshingAccountData ? t('正在读取', 'Reading') : t('尚未检查', 'Not checked'),
      detail: refreshingAccountData
        ? t('正在读取令牌和账单信息', 'Reading token and billing details')
        : t('刷新账号数据后显示令牌与额度状态', 'Refresh account data to see token and quota status'),
    }
  }

  switch (status.quota_state) {
    case 'available':
      return {
        className: 'available',
        label: t('额度可用', 'Quota available'),
        detail: t(
          `已用 ${status.used ?? 0} / ${status.limit ?? unknown}，剩余 ${status.remaining ?? unknown}`,
          `Used ${status.used ?? 0} / ${status.limit ?? unknown}, ${status.remaining ?? unknown} left`,
        ),
      }
    case 'exhausted':
      return {
        className: 'unavailable',
        label: t('额度已用尽', 'Quota exhausted'),
        detail: t(
          `已用 ${status.used ?? status.limit ?? unknown} / ${status.limit ?? unknown}`,
          `Used ${status.used ?? status.limit ?? unknown} / ${status.limit ?? unknown}`,
        ),
      }
    case 'unmetered':
      return {
        className: 'available',
        label: t('令牌可用', 'Token valid'),
        detail: t(
          '令牌有效；当前套餐没有通过接口公开数值额度',
          'Token valid; this plan does not expose numeric quota through the API',
        ),
      }
    case 'restricted':
      return {
        className: 'unavailable',
        label: t('账号受限', 'Account restricted'),
        detail: t('Zed 当前未向该账号开放模型访问', 'Zed has not granted model access to this account'),
      }
    case 'unknown':
      return {
        className: 'warning',
        label: t('额度未知', 'Quota unknown'),
        detail: t('令牌有效，但账单信息暂时没有返回', 'Token valid, but billing details are not available yet'),
      }
    default:
      return {
        className: 'unavailable',
        label: t('令牌不可用', 'Token unavailable'),
        detail: t('LLM 令牌刷新失败，请重新登录该账号', 'LLM token refresh failed; sign in to this account again'),
      }
  }
}

function probeModelName(): string {
  return models.find(model => model.slug === getProbeModel())?.display_name ?? getProbeModel()
}

/**
 * The probe budget depends on the model: Gemini requests carry no cap and
 * GPT-5.5 is floored by the conversion layer, so a fixed "32" would lie.
 * Mirrors probeOutputBudget in src/server.zig.
 */
function probeBudgetLabel(): string {
  const slug = getProbeModel()
  if (slug.startsWith('gemini')) return t('不设输出上限', 'no output cap')
  if (slug.startsWith('gpt-5.5')) return t('最多 32768 个输出 Token', 'at most 32768 output tokens')
  return t('最多 32 个输出 Token', 'at most 32 output tokens')
}

function healthPresentation(status: AccountQuotaStatus | undefined, accountName: string) {
  const checking = checkingAllAccounts || checkingAccounts.has(accountName)
  if (checking) {
    return {
      className: 'checking',
      label: t('正在实测', 'Testing'),
      detail: t(
        `使用 ${probeModelName()}，${probeBudgetLabel()}`,
        `Uses ${probeModelName()} with ${probeBudgetLabel()}`,
      ),
    }
  }

  switch (status?.model_state ?? 'unchecked') {
    case 'healthy': {
      const latency = status?.model_latency_ms ?? 0
      const checkedAt = formatCheckedAt(status?.model_checked_at)
      return {
        className: 'available',
        label: t('模型正常', 'Model healthy'),
        detail: t(`推理耗时 ${latency} 毫秒 · ${checkedAt}`, `Inference took ${latency} ms · ${checkedAt}`),
      }
    }
    case 'auth_error':
      return {
        className: 'unavailable',
        label: t('认证失败', 'Authentication failed'),
        detail: t('真实模型请求未通过账号认证', 'A real model request failed account authentication'),
      }
    case 'rate_limited':
      return {
        className: 'warning',
        label: t('触发限流', 'Rate limited'),
        detail: t(
          '模型路由返回频率或额度限制，账号已进入冷却',
          'Model routing returned a rate or quota limit; the account is now cooling down',
        ),
      }
    case 'upstream_error':
      return {
        className: 'unavailable',
        label: t('上游异常', 'Upstream error'),
        detail: t('低成本推理探测没有完整结束', 'The low-cost inference probe did not finish cleanly'),
      }
    default:
      return {
        className: 'unchecked',
        label: t('尚未实测', 'Not tested yet'),
        detail: t(
          `点击“实测 ${probeModelName()}”后只发送一次最小推理请求`,
          `Click Test ${probeModelName()} to send a single minimal inference request`,
        ),
      }
  }
}

function quotaValue(status: AccountQuotaStatus | undefined): { value: string; detail: string } {
  if (!status) {
    return {
      value: refreshingAccountData ? t('读取中', 'Reading') : t('尚未检查', 'Not checked'),
      detail: t('被动检查不会调用模型', 'Passive checks do not call any model'),
    }
  }
  if (!status.token_ok) return { value: t('不可用', 'Unavailable'), detail: t('令牌刷新失败', 'Token refresh failed') }
  if (status.limit != null && status.limit > 0) {
    return {
      value: t(`剩余 ${status.remaining ?? 0}`, `${status.remaining ?? 0} left`),
      detail: t(
        `已使用 ${status.used ?? 0} / ${status.limit} 次模型请求`,
        `${status.used ?? 0} / ${status.limit} model requests used`,
      ),
    }
  }
  return {
    value: t('Zed 未公开', 'Not disclosed by Zed'),
    detail: status.usage_based_billing
      ? t('已启用按量计费', 'Usage-based billing is enabled')
      : t('精确用量请前往 Zed 账单页面查看', 'See the Zed billing page for exact usage'),
  }
}

function formatUsd(value: number | undefined): string {
  return `$${(value ?? 0).toFixed(2)}`
}

function creditErrorLabel(error?: string): string {
  switch (error) {
    case 'invalid_link':
      return t('链接无效或已被撤销', 'The link is invalid or was revoked')
    case 'timeout':
      return t('Orb 响应超时，稍后重试', 'Orb timed out; try again later')
    case 'parse_error':
      return t('无法解析 Orb 返回的数据', 'Could not parse the Orb response')
    default:
      return t('暂时无法读取 Orb 余额', 'Orb balance is temporarily unavailable')
  }
}

function creditPresentation(credit: AccountCredit | undefined): string {
  if (!credit) {
    return `<p class="credit-note">${t('正在读取 Orb 余额', 'Reading Orb balance')}</p>`
  }
  if (!credit.ok) {
    return `
      <div class="credit-panel">
        <div class="credit-head"><span>${t('Orb 额度', 'Orb credit')}</span><strong class="tone-warning">${t('读取失败', 'Unavailable')}</strong></div>
        <p class="credit-note">${escapeHtml(creditErrorLabel(credit.error))}</p>
      </div>
    `
  }

  // 优先用金额算比例，used_pct 的刻度（0-1 或 0-100）由后端决定，不作假设。
  const ratio = credit.cap_usd && credit.cap_usd > 0 ? ((credit.used_usd ?? 0) / credit.cap_usd) * 100 : credit.used_pct ?? 0
  const pct = Math.min(100, Math.max(0, ratio))
  return `
    <div class="credit-panel">
      <div class="credit-head">
        <span>${t('Orb 额度', 'Orb credit')}</span>
        <strong>${formatUsd(credit.used_usd)} / ${formatUsd(credit.cap_usd)} ${t('已用', 'used')}</strong>
      </div>
      <div class="credit-bar" role="img" aria-label="${t(`已用 ${pct.toFixed(0)}%`, `${pct.toFixed(0)}% used`)}">
        <span style="width:${pct.toFixed(1)}%"></span>
      </div>
      <div class="credit-meta">
        <span>${t('剩余 ', 'Remaining ')}${formatUsd(credit.balance_usd)}</span>
        <span>${t('重置于 ', 'Resets ')}${escapeHtml(formatDate(credit.resets_at))}</span>
      </div>
    </div>
  `
}

function mergeHealthResult(result: AccountHealthResult) {
  const existing = statuses.get(result.name)
  if (existing) statuses.set(result.name, { ...existing, ...result })

  const account = accounts.find(item => item.name === result.name)
  if (account) {
    account.healthy = result.scheduler_healthy
    account.cooldown_s = result.cooldown_s
    account.last_status = result.last_status
  }
}

function renderOverview(checkedAt?: number) {
  const overview = document.getElementById('account-overview')
  if (!overview) return

  const values = [...statuses.values()]
  const usable = values.filter(status => status.usable).length
  const healthy = values.filter(status => status.model_state === 'healthy').length
  const cooling = accounts.filter(account => (statuses.get(account.name)?.cooldown_s ?? account.cooldown_s ?? 0) > 0).length
  const current = accounts.find(account => account.current)?.name ?? t('未选择', 'None selected')
  const checkedValue = checkedAt ?? lastCheckedAt
  const checked = checkedValue
    ? new Date(checkedValue * 1000).toLocaleTimeString(locale, { hour12: false })
    : t('等待刷新', 'Waiting for refresh')

  overview.innerHTML = `
    <article class="metric-card featured">
      <span class="metric-label">${t('当前调度账号', 'Current scheduling account')}</span>
      <strong>${escapeHtml(current)}</strong>
      <small>${t('新请求会优先使用此账号', 'New requests prefer this account')}</small>
    </article>
    <article class="metric-card">
      <span class="metric-label">${t('令牌可用', 'Token valid')}</span>
      <strong class="metric-good">${statuses.size ? `${usable} / ${accounts.length}` : '--'}</strong>
      <small>${t('被动令牌检查结果', 'Passive token check result')}</small>
    </article>
    <article class="metric-card">
      <span class="metric-label">${escapeHtml(t(`${probeModelName()} 实测正常`, `${probeModelName()} healthy`))}</span>
      <strong class="metric-good">${statuses.size ? `${healthy} / ${accounts.length}` : '--'}</strong>
      <small>${t('真实最小推理请求结果', 'Result of a real minimal inference request')}</small>
    </article>
    <article class="metric-card">
      <span class="metric-label">${t('冷却中', 'Cooling down')}</span>
      <strong class="${cooling ? 'metric-warn' : ''}">${cooling}</strong>
      <small>${t('最近刷新：', 'Last refreshed: ')}${escapeHtml(checked)}</small>
    </article>
  `
}

function renderAccountCards() {
  const list = document.getElementById('account-list')
  if (!list) return

  if (accounts.length === 0) {
    list.innerHTML = `
      <div class="empty-state">
        <div class="empty-state-icon">${icons.users}</div>
        <strong>${t('还没有可用账号', 'No accounts yet')}</strong>
        <p>${t('通过下方 GitHub OAuth 登录按钮添加第一个 Zed 账号。', 'Add your first Zed account with the GitHub OAuth button below.')}</p>
      </div>
    `
    return
  }

  list.innerHTML = accounts.map((account, index) => {
    const status = statuses.get(account.name)
    const quota = quotaPresentation(status)
    const health = healthPresentation(status, account.name)
    const balance = quotaValue(status)
    const cooldown = status?.cooldown_s ?? account.cooldown_s ?? 0
    const checking = checkingAllAccounts || checkingAccounts.has(account.name)

    return `
      <article class="account-card ${account.current ? 'active' : ''}">
        <header class="account-card-header">
          <div class="account-identity">
            <span class="account-avatar">${escapeHtml(account.name.slice(0, 1).toUpperCase())}</span>
            <div>
              <div class="account-name-line">
                <strong>${escapeHtml(account.name)}</strong>
                ${account.current ? `<span class="state-pill current">${t('当前账号', 'Current account')}</span>` : ''}
              </div>
              <span class="account-id">${t('用户编号 ', 'User ID ')}${escapeHtml(account.user_id)}</span>
            </div>
          </div>
          <div class="account-state-group">
            <span class="state-pill ${quota.className}">${escapeHtml(quota.label)}</span>
            <span class="state-pill ${health.className}">${escapeHtml(health.label)}</span>
          </div>
        </header>

        <div class="account-detail-grid">
          <div class="detail-cell">
            <span>${t('套餐', 'Plan')}</span>
            <strong>${escapeHtml(formatPlan(status?.plan))}</strong>
            <small>${status?.billing_ok ? t('已读取账单信息', 'Billing details loaded') : escapeHtml(quota.detail)}</small>
          </div>
          <div class="detail-cell">
            <span>${t('到期或续订', 'Expires / renews')}</span>
            <strong>${escapeHtml(formatDate(status?.subscription_ends_at))}</strong>
            <small>${status?.overdue ? t('Zed 返回账单逾期提示', 'Zed reports an overdue bill') : t('来自 Zed 账号接口', 'From the Zed account API')}</small>
          </div>
          <div class="detail-cell">
            <span>${t('额度', 'Quota')}</span>
            <strong>${escapeHtml(balance.value)}</strong>
            <small>${escapeHtml(balance.detail)}</small>
          </div>
          <div class="detail-cell">
            <span>${t('模型链路', 'Model path')}</span>
            <strong class="tone-${health.className}">${escapeHtml(health.label)}</strong>
            <small>${escapeHtml(health.detail)}${cooldown > 0 ? t(` · 冷却 ${cooldown} 秒`, ` · ${cooldown}s cooldown`) : ''}</small>
          </div>
        </div>

        <div class="account-credit">
          ${status?.orb_configured
            ? creditPresentation(status.credit)
            : `
              <div class="orb-link-row">
                <span class="credit-label">${t('关联 Orb 余额', 'Link Orb balance')}</span>
                <input type="url" class="orb-link-input" data-index="${index}" autocomplete="off" spellcheck="false" placeholder="${t('粘贴 Orb 门户链接（含 token 参数）', 'Paste the Orb portal link (with the token parameter)')}" />
                <button class="button primary orb-save-button" data-index="${index}" type="button">${t('保存', 'Save')}</button>
              </div>
              <span class="orb-link-help">${t('粘贴 Orb 门户链接即可显示已用额度和重置日期；链接等同凭据，只保存在本机，页面不会回显。', 'Paste an Orb portal link to see usage and the reset date. The link is credential-equivalent, stored only locally, and never shown again.')}</span>
            `}
        </div>

        <footer class="account-card-footer">
          <a href="https://zed.dev/account/billing" target="_blank" rel="noreferrer">
            ${t('查看 Zed 账单', 'View Zed billing')} ${icons.externalLink}
          </a>
          <div>
            <button class="button secondary health-button" data-index="${index}" type="button" ${checking ? 'disabled' : ''}>
              ${checking ? `<span class="spinner"></span>${t('检测中', 'Testing')}` : `${icons.activity} ${escapeHtml(t(`实测 ${probeModelName()}`, `Test ${probeModelName()}`))}`}
            </button>
            ${status?.orb_configured
              ? `<button class="button secondary orb-unlink-button" data-index="${index}" type="button">${t('取消关联', 'Unlink')}</button>`
              : ''}
            ${account.current
              ? `<span class="active-account-hint">${icons.check} ${t('已参与优先调度', 'In the priority pool')}</span>`
              : `<button class="button primary switch-button" data-index="${index}" type="button">${t('切换到此账号', 'Switch to this account')}</button>`}
          </div>
        </footer>
      </article>
    `
  }).join('')

  list.querySelectorAll<HTMLButtonElement>('.switch-button').forEach(button => {
    button.addEventListener('click', async () => {
      const account = accounts[Number(button.dataset.index)]
      if (!account) return
      button.disabled = true
      try {
        await switchAccount(account.name)
        showToast(t(`已切换到账号：${account.name}`, `Switched to account: ${account.name}`))
        await loadAccounts()
      } catch (error) {
        const message = error instanceof Error ? error.message : t('未知错误', 'Unknown error')
        showToast(t(`账号切换失败：${message}`, `Failed to switch account: ${message}`))
        button.disabled = false
      }
    })
  })

  list.querySelectorAll<HTMLButtonElement>('.health-button').forEach(button => {
    button.addEventListener('click', () => {
      const account = accounts[Number(button.dataset.index)]
      if (account) void runHealthCheck(account.name)
    })
  })

  list.querySelectorAll<HTMLButtonElement>('.orb-save-button').forEach(button => {
    button.addEventListener('click', () => {
      const index = Number(button.dataset.index)
      const account = accounts[index]
      const input = document.querySelector<HTMLInputElement>(`.orb-link-input[data-index="${index}"]`)
      if (!account || !input) return
      void saveOrbLink(account.name, input.value.trim(), button)
    })
  })

  list.querySelectorAll<HTMLButtonElement>('.orb-unlink-button').forEach(button => {
    button.addEventListener('click', () => {
      const account = accounts[Number(button.dataset.index)]
      if (account) void saveOrbLink(account.name, '', button)
    })
  })
}

async function saveOrbLink(accountName: string, url: string, button: HTMLButtonElement) {
  if (!url && button.classList.contains('orb-save-button')) {
    showToast(t('请先粘贴 Orb 门户链接', 'Paste the Orb portal link first'))
    return
  }
  // 不把输入框里的链接写进 toast、日志或 DOM 的其它位置。
  button.disabled = true
  try {
    await setAccountOrb(accountName, url)
    showToast(url
      ? t(`已保存 ${accountName} 的 Orb 链接`, `Saved the Orb link for ${accountName}`)
      : t(`已取消 ${accountName} 的 Orb 关联`, `Unlinked Orb for ${accountName}`))
    // 余额要等后台把 orb_url 写回账号并拉取，先按新状态重绘再刷新。
    const status = statuses.get(accountName)
    if (status) statuses.set(accountName, { ...status, orb_configured: Boolean(url), credit: undefined })
    renderAccountCards()
    await refreshAccountData(true)
  } catch (error) {
    const code = error instanceof Error ? error.message : ''
    showToast(code === 'invalid orb link'
      ? t('Orb 链接无效：需要 https://portal.withorb.com 且包含 token 参数', 'Invalid Orb link: it must be an https://portal.withorb.com URL with a token parameter')
      : t(`Orb 链接保存失败：${code || t('未知错误', 'Unknown error')}`, `Failed to save the Orb link: ${code || t('unknown error', 'unknown error')}`))
    button.disabled = false
  }
}

function updateActionButtons() {
  const refreshButton = document.getElementById('refresh-accounts-button') as HTMLButtonElement | null
  if (refreshButton) {
    refreshButton.disabled = refreshingAccountData || checkingAllAccounts
    refreshButton.innerHTML = refreshingAccountData
      ? `<span class="spinner"></span>${t('正在刷新', 'Refreshing')}`
      : `${icons.refresh} ${t('刷新账号数据', 'Refresh account data')}`
  }

  const healthButton = document.getElementById('check-all-button') as HTMLButtonElement | null
  if (healthButton) {
    healthButton.disabled = checkingAllAccounts || refreshingAccountData || accounts.length === 0
    healthButton.innerHTML = checkingAllAccounts
      ? `<span class="spinner"></span>${t('正在实测全部账号', 'Testing all accounts')}`
      : `${icons.activity} ${t('全部账号实测', 'Test all accounts')}`
  }

  const orbButton = document.getElementById('refresh-orb-button') as HTMLButtonElement | null
  if (orbButton) {
    const hasLink = accounts.some(account => statuses.get(account.name)?.orb_configured)
    orbButton.disabled = refreshingOrb || !hasLink
    orbButton.title = hasLink ? '' : t('没有账号关联 Orb 链接', 'No account has an Orb link yet')
    orbButton.innerHTML = refreshingOrb
      ? `<span class="spinner"></span>${t('正在获取额度', 'Refreshing balance')}`
      : `${icons.refresh} ${t('获取最新额度', 'Refresh balance')}`
  }
}

/**
 * 强制重新拉取全部已关联账号的 Orb 余额。失败或限流时保留卡片上的旧数字，
 * 只更新响应里带回的账号，并用 toast 说明结果。
 */
async function refreshOrbBalances() {
  if (refreshingOrb) return
  refreshingOrb = true
  updateActionButtons()
  try {
    const result = await refreshOrbCredit()
    const entries = result.accounts ?? (result.account
      ? [{ account: result.account, refreshed: result.refreshed, retry_after_s: result.retry_after_s ?? 0, credit: result.credit! }]
      : [])
    let updated = 0
    let failed = 0
    let retryAfter = 0
    let failureCode = ''
    for (const entry of entries) {
      const status = statuses.get(entry.account)
      if (!status) continue
      retryAfter = Math.max(retryAfter, entry.retry_after_s ?? 0)
      // 只有成功值才写回；credit.ok=false 表示本次没取到新值，卡片保留旧数字，
      // 由 toast 报错误码（80 §4.4“失败保留旧值”）。
      if (entry.credit?.ok) {
        statuses.set(entry.account, { ...status, credit: entry.credit })
        updated += 1
      } else {
        failed += 1
        failureCode = failureCode || entry.credit?.error || ''
      }
    }
    renderAccountCards()
    const retryNote = t(`获取过于频繁，${retryAfter} 秒后可再次刷新`, `Refreshed too recently; try again in ${retryAfter}s`)
    if (entries.length === 0) showToast(t('没有关联 Orb 链接的账号', 'No account has an Orb link'))
    else if (retryAfter > 0 && updated === 0) showToast(retryNote)
    else if (failed > 0) {
      const reason = creditErrorLabel(failureCode)
      showToast(t(
        `${failed} 个账号的额度刷新失败（${reason}），已保留原显示${retryAfter > 0 ? `；${retryAfter} 秒后可再次刷新` : ''}`,
        `Balance refresh failed for ${failed} account(s) (${reason}); the previous values are kept${retryAfter > 0 ? `; try again in ${retryAfter}s` : ''}`,
      ))
    } else if (retryAfter > 0) showToast(retryNote)
    else showToast(t(`已更新 ${updated} 个账号的 Orb 额度`, `Refreshed the Orb balance for ${updated} account(s)`))
  } catch (error) {
    // 网络/非 2xx：不清空任何已有余额。
    const message = error instanceof Error ? error.message : t('未知错误', 'Unknown error')
    showToast(t(`获取最新额度失败：${message}`, `Failed to refresh the balance: ${message}`))
  } finally {
    refreshingOrb = false
    updateActionButtons()
  }
}

async function refreshAccountData(silent = false) {
  if (refreshingAccountData || accounts.length === 0) return
  refreshingAccountData = true
  updateActionButtons()
  renderAccountCards()

  try {
    const response = await fetchAccountStatuses()
    statuses = new Map(response.accounts.map(status => [status.name, status]))
    lastCheckedAt = response.checked_at
    renderOverview(response.checked_at)
    renderAccountCards()
    if (!silent)
      showToast(
        t(
          '账号令牌、套餐与额度信息已刷新；本操作没有调用模型',
          'Account token, plan, and quota details refreshed; no model was called',
        ),
      )
  } catch (error) {
    if (!silent) {
      const message = error instanceof Error ? error.message : t('未知错误', 'Unknown error')
      showToast(t(`账号数据刷新失败：${message}`, `Failed to refresh account data: ${message}`))
    }
  } finally {
    refreshingAccountData = false
    updateActionButtons()
    renderAccountCards()
  }
}

async function runHealthCheck(accountName?: string) {
  if (checkingAllAccounts || (accountName && checkingAccounts.has(accountName))) return
  if (accountName) checkingAccounts.add(accountName)
  else checkingAllAccounts = true
  updateActionButtons()
  renderAccountCards()

  try {
    const response = await checkAccountHealth(accountName, getProbeModel())
    response.accounts.forEach(mergeHealthResult)
    renderOverview()
    const passed = response.accounts.filter(result => result.model_ok).length
    showToast(
      t(
        `账号实测完成：${passed}/${response.accounts.length} 通过 · ${response.probe.model} · ${response.probe.reasoning_effort ?? t('无推理档位', 'no effort knob')}`,
        `Account test complete: ${passed}/${response.accounts.length} passed · ${response.probe.model} · ${response.probe.reasoning_effort ?? 'no effort knob'}`,
      ),
    )
  } catch (error) {
    const message = error instanceof Error ? error.message : t('未知错误', 'Unknown error')
    showToast(t(`账号实测失败：${message}`, `Account test failed: ${message}`))
  } finally {
    if (accountName) checkingAccounts.delete(accountName)
    else checkingAllAccounts = false
    updateActionButtons()
    renderAccountCards()
  }
}

async function loadAccounts() {
  const list = document.getElementById('account-list')
  if (!list) return

  try {
    const data = await fetchAccounts()
    accounts = data.accounts ?? []
    const count = document.getElementById('acc-count')
    if (count) count.textContent = String(accounts.length)
    renderOverview()
    renderAccountCards()
    updateActionButtons()
    if (accounts.length > 0) void refreshAccountData(true)
  } catch (error) {
    const message = error instanceof Error ? escapeHtml(error.message) : t('未知错误', 'Unknown error')
    list.innerHTML = `<div class="error-state">${t(`账号加载失败：${message}`, `Failed to load accounts: ${message}`)}</div>`
  }
}

export function renderAccounts() {
  const page = document.getElementById('page-accounts')!
  page.innerHTML = `
    <div class="page-heading">
      <div>
        <span class="eyebrow">${t('账号与故障转移', 'Accounts & failover')}</span>
        <h1>${t('账号中心', 'Account center')}</h1>
        <p>${t('集中查看令牌、套餐、额度、冷却状态和真实模型链路，并控制当前优先调度账号。', 'Review tokens, plans, quota, cooldowns, and real model paths in one place, and control which account is scheduled first.')}</p>
      </div>
      <div class="heading-actions">
        <label class="probe-model-picker">
          <span>${t('实测模型', 'Test model')}</span>
          <select id="probe-model-select" aria-label="${t('实测模型', 'Test model')}"></select>
        </label>
        <button class="button secondary" id="refresh-accounts-button" type="button">${icons.refresh} ${t('刷新账号数据', 'Refresh account data')}</button>
        <button class="button secondary" id="refresh-orb-button" type="button">${icons.refresh} ${t('获取最新额度', 'Refresh balance')}</button>
        <button class="button primary" id="check-all-button" type="button">${icons.activity} ${t('全部账号实测', 'Test all accounts')}</button>
      </div>
    </div>

    <div class="notice-banner">
      <span>${icons.shield}</span>
      <p><strong>${t('检测成本说明', 'About check cost')}</strong><span id="probe-cost-note"></span></p>
    </div>

    <div class="metric-grid" id="account-overview"></div>

    <div class="section-heading">
      <div><span>${t('账号列表', 'Accounts')}</span><p>${t('健康账号优先，异常账号会按错误类型自动进入冷却。', 'Healthy accounts come first; failing accounts cool down automatically based on the error type.')}</p></div>
    </div>
    <div class="account-list" id="account-list">
      <div class="loading-panel"><span class="spinner"></span>${t('正在加载账号', 'Loading accounts')}</div>
    </div>

    <button class="add-account-card" id="add-account-button" type="button">
      <span class="add-account-icon">${icons.plus}</span>
      <span><strong>${t('添加 Zed 账号', 'Add a Zed account')}</strong><small>${t('通过 GitHub OAuth 完成登录，不在页面中输入或保存密码', 'Sign in with GitHub OAuth; no password is entered or stored on this page')}</small></span>
      <span class="add-account-arrow">${icons.arrowRight}</span>
    </button>
    <div class="login-banner" id="login-banner" hidden></div>
  `

  document.getElementById('add-account-button')!.addEventListener('click', () => void startOAuthLogin())
  document.getElementById('refresh-accounts-button')!.addEventListener('click', () => void refreshAccountData())
  document.getElementById('refresh-orb-button')!.addEventListener('click', () => void refreshOrbBalances())
  document.getElementById('check-all-button')!.addEventListener('click', () => void runHealthCheck())

  const select = document.getElementById('probe-model-select') as HTMLSelectElement
  renderProbeModelOptions(select, models)
  select.addEventListener('change', () => {
    setProbeModel(select.value)
    updateProbeCopy()
  })
  onProbeModelChange(() => {
    renderProbeModelOptions(select, models)
    updateProbeCopy()
  })

  updateProbeCopy()
  void loadAccountModels()
  void loadAccounts()
}

/** 把当前实测模型写进按钮、提示和横幅；模型目录后到达时再刷新一次。 */
function updateProbeCopy() {
  const note = document.getElementById('probe-cost-note')
  if (note) {
    note.textContent = t(
      `：令牌、套餐和额度检查不会调用模型；账号实测只会对每个选中账号调用一次 ${probeModelName()}，使用短提示词和${probeBudgetLabel()}，不会轮流检测其他模型。`,
      `: Token, plan, and quota checks do not call any model. Account tests call ${probeModelName()} once per selected account with a short prompt and ${probeBudgetLabel()}; other models are not tested in turn.`,
    )
  }
  if (accounts.length === 0) return
  renderOverview(lastCheckedAt)
  renderAccountCards()
}

async function loadAccountModels() {
  const select = document.getElementById('probe-model-select') as HTMLSelectElement | null
  if (!select) return
  try {
    const data = await loadCatalog()
    models = catalogModels(data)
    renderProbeModelOptions(select, models)
    updateProbeCopy()
  } catch {
    // 模型目录不可用时保留下拉框里的默认项，实测仍会显式发送默认模型。
  }
}

async function startOAuthLogin() {
  const banner = document.getElementById('login-banner')!
  const button = document.getElementById('add-account-button') as HTMLButtonElement
  button.disabled = true
  banner.hidden = false
  banner.innerHTML = `<span class="spinner"></span><span>${t('正在生成密钥并启动 GitHub OAuth…', 'Generating a key and starting GitHub OAuth…')}</span>`

  try {
    const data = await startLogin(undefined, 'paste')
    if (data.error) {
      banner.innerHTML = `<span class="error-text">${icons.xCircle} ${escapeHtml(data.error)}</span>`
      button.disabled = false
      return
    }

    banner.innerHTML = `
      <span>${t('授权链接 10 分钟内有效', 'The authorization link is valid for 10 minutes')}</span>
      ${data.login_url ? `<a id="login-link" target="_blank" rel="noreferrer">${t('打开授权页面', 'Open the authorization page')} ${icons.externalLink}</a>` : ''}
      <span>${t('登录后若浏览器显示无法访问此页面，复制地址栏完整网址粘贴到这里', 'If the browser says this page cannot be reached after sign-in, copy the full URL from the address bar and paste it here')}</span>
      <div class="paste-row">
        <input id="paste-url-input" type="text" autocomplete="off" placeholder="${t('粘贴浏览器地址栏里的完整网址', 'Paste the full URL from the address bar')}" />
        <button class="button primary" id="paste-complete-button" type="button">${t('完成登录', 'Complete sign-in')}</button>
      </div>
      <span class="error-text" id="paste-error" hidden></span>
    `

    const link = document.getElementById('login-link') as HTMLAnchorElement | null
    if (link && data.login_url) link.href = data.login_url

    const input = document.getElementById('paste-url-input') as HTMLInputElement
    const submit = document.getElementById('paste-complete-button') as HTMLButtonElement
    const errorText = document.getElementById('paste-error')!

    const finish = async () => {
      const url = input.value.trim()
      if (!url) {
        errorText.hidden = false
        errorText.innerHTML = `${icons.xCircle} ${t('请先粘贴地址栏完整网址', 'Paste the full URL from the address bar first')}`
        return
      }
      submit.disabled = true
      errorText.hidden = true
      try {
        await completeLogin(url)
        statuses.clear()
        showToast(t('Zed 账号添加成功', 'Zed account added'))
        await loadAccounts()
        banner.innerHTML = `${icons.checkCircle}<span>${t('账号登录成功，正在刷新账号列表。', 'Signed in successfully; refreshing the account list.')}</span>`
        button.disabled = false
        window.setTimeout(() => { banner.hidden = true }, 3000)
      } catch (error) {
        errorText.hidden = false
        const message = escapeHtml(error instanceof Error ? error.message : t('未知错误', 'Unknown error'))
        errorText.innerHTML = `${icons.xCircle} ${message}`
        submit.disabled = false
      }
    }

    submit.addEventListener('click', () => void finish())
    input.addEventListener('keydown', event => {
      if (event.key === 'Enter') {
        event.preventDefault()
        void finish()
      }
    })
  } catch (error) {
    const message = error instanceof Error ? escapeHtml(error.message) : t('未知错误', 'Unknown error')
    banner.innerHTML = `<span class="error-text">${icons.xCircle} ${t(`登录启动失败：${message}`, `Failed to start sign-in: ${message}`)}</span>`
    button.disabled = false
  }
}
