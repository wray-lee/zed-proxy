import './style.css'
import { authFetch } from './api'
import { icons } from './icons'
import { lang, locale, setLang, t } from './i18n'
import { renderAccounts } from './pages/accounts'
import { renderEndpoints } from './pages/endpoints'
import { renderHealth } from './pages/health'
import { renderIntegration } from './pages/integration'

type PageId = 'accounts' | 'health' | 'endpoints' | 'integration'

const PAGE_TITLES: Record<PageId, string> = {
  accounts: t('账号中心', 'Account center'),
  health: t('服务检查', 'Health check'),
  endpoints: t('接口清单', 'Endpoints'),
  integration: t('客户端接入', 'Client setup'),
}

const app = document.getElementById('app')!
const port = location.port || '8001'

document.title = t('ZedProxy 控制台', 'ZedProxy Console')
document.querySelector('meta[name="description"]')?.setAttribute(
  'content',
  t('ZedProxy 本地模型网关控制台', 'ZedProxy local model gateway console'),
)

app.innerHTML = `
  <div class="app-shell">
    <aside class="side-panel">
      <div class="brand-block">
        <div class="brand-mark">${icons.zap}</div>
        <div>
          <strong>ZedProxy</strong>
          <span>${t('本地模型接入控制台', 'Local model gateway console')}</span>
        </div>
      </div>

      <div class="service-overview" id="service-overview">
        <div class="service-overview-head">
          <span class="live-dot checking" id="service-dot"></span>
          <span id="service-label">${t('正在检查服务', 'Checking service')}</span>
        </div>
        <div class="service-overview-meta">
          <span>127.0.0.1:${port}</span>
          <span id="model-count">${t('-- 个模型', '-- models')}</span>
        </div>
      </div>

      <nav class="primary-nav" aria-label="${t('主导航', 'Main navigation')}">
        <section class="nav-section">
          <p>${t('运行管理', 'Operations')}</p>
          <button class="nav-item active" data-page="accounts" type="button">
            <span class="nav-icon">${icons.users}</span>
            <span>${t('账号中心', 'Account center')}</span>
            <span class="nav-badge" id="acc-count">0</span>
          </button>
          <button class="nav-item" data-page="health" type="button">
            <span class="nav-icon">${icons.activity}</span>
            <span>${t('服务检查', 'Health check')}</span>
          </button>
        </section>

        <section class="nav-section">
          <p>${t('开发接入', 'Development')}</p>
          <button class="nav-item" data-page="endpoints" type="button">
            <span class="nav-icon">${icons.globe}</span>
            <span>${t('接口清单', 'Endpoints')}</span>
          </button>
          <button class="nav-item" data-page="integration" type="button">
            <span class="nav-icon">${icons.code}</span>
            <span>${t('客户端接入', 'Client setup')}</span>
          </button>
        </section>
      </nav>

      <footer class="side-footer">
        <div>${icons.shield}</div>
        <p><strong>${t('仅监听本机地址', 'Listening on localhost only')}</strong><span>${t('账号凭据不会发送到前端页面', 'Credentials are never sent to the browser')}</span></p>
      </footer>
    </aside>

    <section class="workspace">
      <header class="topbar">
        <div>
          <span class="topbar-kicker">${t('控制台', 'Console')} / <span id="current-page-name">${PAGE_TITLES.accounts}</span></span>
          <strong id="topbar-title">${PAGE_TITLES.accounts}</strong>
        </div>
        <div class="topbar-status">
          <span class="topbar-clock" id="local-clock"></span>
          <span class="runtime-chip">${icons.server} ${t('服务端口', 'Service port')} ${port}</span>
          <div class="lang-switch" role="group" aria-label="${t('语言', 'Language')}">
            <button class="lang-button${lang === 'zh' ? ' active' : ''}" type="button" data-lang="zh" lang="zh-CN" aria-pressed="${lang === 'zh'}"${lang === 'zh' ? ' aria-current="true"' : ''}>中文</button>
            <button class="lang-button${lang === 'en' ? ' active' : ''}" type="button" data-lang="en" lang="en" aria-pressed="${lang === 'en'}"${lang === 'en' ? ' aria-current="true"' : ''}>EN</button>
          </div>
        </div>
      </header>

      <main class="page-stage">
        <section class="page active" id="page-accounts"></section>
        <section class="page" id="page-health"></section>
        <section class="page" id="page-endpoints"></section>
        <section class="page" id="page-integration"></section>
      </main>
    </section>
  </div>
  <div class="toast" id="toast" role="status" aria-live="polite"></div>
`

function isPageId(value: string | undefined): value is PageId {
  return value != null && value in PAGE_TITLES
}

function activatePage(pageId: PageId, updateHash = true) {
  document.querySelectorAll<HTMLButtonElement>('.nav-item[data-page]').forEach(button => {
    const active = button.dataset.page === pageId
    button.classList.toggle('active', active)
    if (active) button.setAttribute('aria-current', 'page')
    else button.removeAttribute('aria-current')
  })

  document.querySelectorAll<HTMLElement>('.page').forEach(page => {
    page.classList.toggle('active', page.id === `page-${pageId}`)
  })

  const title = PAGE_TITLES[pageId]
  document.getElementById('current-page-name')!.textContent = title
  document.getElementById('topbar-title')!.textContent = title
  document.title = t(`${title} · ZedProxy`, `${title} · ZedProxy Console`)
  if (updateHash) history.replaceState(null, '', `#${pageId}`)
}

document.querySelectorAll<HTMLButtonElement>('.nav-item[data-page]').forEach(button => {
  button.addEventListener('click', () => {
    const pageId = button.dataset.page
    if (isPageId(pageId)) activatePage(pageId)
  })
})

document.querySelectorAll<HTMLButtonElement>('.lang-button').forEach(button => {
  button.addEventListener('click', () => {
    const next = button.dataset.lang
    if ((next === 'zh' || next === 'en') && next !== lang) setLang(next)
  })
})

window.addEventListener('hashchange', () => {
  const pageId = location.hash.slice(1)
  if (isPageId(pageId)) activatePage(pageId, false)
})

function updateClock() {
  document.getElementById('local-clock')!.textContent = new Intl.DateTimeFormat(locale, {
    month: '2-digit',
    day: '2-digit',
    hour: '2-digit',
    minute: '2-digit',
    second: '2-digit',
    hour12: false,
  }).format(new Date())
}

async function refreshServiceState() {
  const dot = document.getElementById('service-dot')!
  const label = document.getElementById('service-label')!
  const count = document.getElementById('model-count')!
  dot.className = 'live-dot checking'
  label.textContent = t('正在检查服务', 'Checking service')

  try {
    const response = await authFetch('/v1/models', { cache: 'no-store' })
    if (!response.ok) throw new Error(`HTTP ${response.status}`)
    const data = await response.json() as { data?: unknown[] }
    const modelCount = data.data?.length ?? 0
    dot.className = 'live-dot online'
    label.textContent = t('本地服务运行中', 'Local service running')
    count.textContent = t(`${modelCount} 个模型`, `${modelCount} ${modelCount === 1 ? 'model' : 'models'}`)
  } catch {
    dot.className = 'live-dot offline'
    label.textContent = t('本地服务未响应', 'Local service not responding')
    count.textContent = t('连接失败', 'Connection failed')
  }
}

renderAccounts()
renderHealth()
renderEndpoints()
renderIntegration()

const initialPage = location.hash.slice(1)
activatePage(isPageId(initialPage) ? initialPage : 'accounts', false)
updateClock()
window.setInterval(updateClock, 1000)
void refreshServiceState()
document.addEventListener('visibilitychange', () => {
  if (document.visibilityState === 'visible') void refreshServiceState()
})
