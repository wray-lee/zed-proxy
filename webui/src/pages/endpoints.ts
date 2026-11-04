import { t } from '../i18n'

type GroupKey = 'protocol' | 'accounts' | 'compat'

interface EndpointDefinition {
  method: 'GET' | 'POST'
  path: string
  description: string
  group: GroupKey
}

const ENDPOINTS: EndpointDefinition[] = [
  { method: 'POST', path: '/v1/responses', description: t('面向 Codex 的 OpenAI Responses API，支持流式和非流式响应', 'OpenAI Responses API for Codex, with streaming and non-streaming responses'), group: 'protocol' },
  { method: 'POST', path: '/v1/chat/completions', description: t('OpenAI Chat Completions 兼容接口', 'OpenAI Chat Completions compatible endpoint'), group: 'protocol' },
  { method: 'POST', path: '/v1/messages', description: t('面向 Claude Code 的 Anthropic Messages 原生接口', 'Native Anthropic Messages API for Claude Code'), group: 'protocol' },
  { method: 'POST', path: '/v1/messages/count_tokens', description: t('Claude Code 启动阶段的 Token 计数兼容接口', 'Token counting compatibility endpoint used during Claude Code startup'), group: 'compat' },
  { method: 'GET', path: '/v1/models', description: t('返回当前网关可用的模型列表', 'Returns the model list available from this gateway'), group: 'compat' },
  { method: 'POST', path: '/api/event_logging/batch', description: t('Claude Code 事件上报兼容占位接口', 'Placeholder compatibility endpoint for Claude Code event reporting'), group: 'compat' },
  { method: 'GET', path: '/zed/accounts', description: t('读取脱敏后的账号列表与当前调度账号', 'Reads the redacted account list and the currently scheduled account'), group: 'accounts' },
  { method: 'GET', path: '/zed/accounts/status', description: t('被动检查全部账号的令牌、套餐和额度', 'Passively checks tokens, plans and quota for all accounts'), group: 'accounts' },
  { method: 'POST', path: '/zed/accounts/health', description: t('对单个或全部账号执行低成本模型实测', 'Runs a low-cost live model check on one or all accounts'), group: 'accounts' },
  { method: 'POST', path: '/zed/accounts/switch', description: t('切换当前优先调度账号', 'Switches the preferred scheduling account'), group: 'accounts' },
  { method: 'GET', path: '/zed/usage', description: t('读取当前账号的套餐与用量信息', 'Reads plan and usage info for the current account'), group: 'accounts' },
  { method: 'GET', path: '/zed/billing', description: t('读取当前账号的 Zed 账单响应', 'Reads the Zed billing response for the current account'), group: 'accounts' },
  { method: 'POST', path: '/zed/login', description: t('启动 GitHub OAuth 登录流程', 'Starts the GitHub OAuth login flow'), group: 'accounts' },
  { method: 'GET', path: '/zed/login/status', description: t('查询 GitHub OAuth 登录流程状态', 'Polls the status of the GitHub OAuth login flow'), group: 'accounts' },
]

const PROTOCOLS = [
  { name: 'Responses', client: t('Codex 首选', 'Preferred for Codex'), path: '/v1/responses', tone: 'cyan' },
  { name: 'Chat Completions', client: t('通用 OpenAI 客户端', 'Generic OpenAI clients'), path: '/v1/chat/completions', tone: 'blue' },
  { name: 'Messages', client: t('Claude Code 首选', 'Preferred for Claude Code'), path: '/v1/messages', tone: 'violet' },
]

const GROUPS: { key: GroupKey; label: string; hint: string }[] = [
  { key: 'protocol', label: t('模型协议', 'Model protocols'), hint: t('模型请求入口', 'Model request entry points') },
  { key: 'accounts', label: t('账号管理', 'Account management'), hint: t('账号、额度与调度接口', 'Accounts, quota and scheduling') },
  { key: 'compat', label: t('兼容接口', 'Compatibility'), hint: t('客户端启动与兼容接口', 'Client startup and compatibility endpoints') },
]

export function renderEndpoints() {
  const page = document.getElementById('page-endpoints')!

  page.innerHTML = `
    <div class="page-heading">
      <div>
        <span class="eyebrow">${t('协议与路由', 'Protocols & routes')}</span>
        <h1>${t('接口清单', 'Endpoints')}</h1>
        <p>${t('网关同时提供 Responses、Chat Completions 和 Anthropic Messages 三套请求方式。', 'The gateway exposes three request styles: Responses, Chat Completions and Anthropic Messages.')}</p>
      </div>
    </div>

    <div class="protocol-grid">
      ${PROTOCOLS.map(protocol => `
        <article class="protocol-card ${protocol.tone}">
          <span>${protocol.client}</span>
          <strong>${protocol.name}</strong>
          <code>${protocol.path}</code>
        </article>
      `).join('')}
    </div>

    ${GROUPS.map(group => `
      <section class="endpoint-section">
        <div class="section-heading"><div><span>${group.label}</span><p>${group.hint}</p></div></div>
        <div class="endpoint-table">
          ${ENDPOINTS.filter(endpoint => endpoint.group === group.key).map(endpoint => `
            <div class="endpoint-row">
              <span class="method-badge ${endpoint.method.toLowerCase()}">${endpoint.method}</span>
              <code>${endpoint.path}</code>
              <p>${endpoint.description}</p>
            </div>
          `).join('')}
        </div>
      </section>
    `).join('')}
  `
}
