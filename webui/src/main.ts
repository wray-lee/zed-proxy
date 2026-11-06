// Boot order: framework first, preferences second (tolerantly), pages third,
// global listeners last. Nothing before the framework may throw uncaught.

import './theme.css'
import { clear, h, pageRoot } from './dom.ts'
import { detectLanguage, currentPort, loadStoredPreferences, pageFromHash, PAGES, state, subscribe, announce, saveLanguage } from './state.ts'
import type { PageId } from './state.ts'
import { tr } from './text.ts'
import { checkLiveness, loadCatalogue } from './data.ts'
import { toast } from './ui.ts'
import { attachAccountsPage, accountsSummary } from './pages/accounts.ts'
import { attachModelsPage } from './pages/models.ts'
import { attachHealthPage } from './pages/health.ts'
import { attachRoutesPage } from './pages/routes.ts'
import { attachClientsPage } from './pages/clients.ts'
import { onChange, reloadAccounts } from './store.ts'

interface PageModule {
  render(): void
  /** Runs once after mount to load first-screen data. */
  attach(): void
}

const modules: Record<PageId, PageModule> = {
  accounts: { render: attachAccountsPage.render, attach: attachAccountsPage.attach },
  models: { render: attachModelsPage.render, attach: attachModelsPage.attach },
  health: { render: attachHealthPage.render, attach: attachHealthPage.attach },
  endpoints: { render: attachRoutesPage.render, attach: attachRoutesPage.attach },
  integration: { render: attachClientsPage.render, attach: attachClientsPage.attach },
}

const PAGE_LABELS: Record<PageId, 'navAccounts' | 'navModels' | 'navDiagnostics' | 'navRoutes' | 'navConnect'> = {
  accounts: 'navAccounts',
  models: 'navModels',
  health: 'navDiagnostics',
  endpoints: 'navRoutes',
  integration: 'navConnect',
}

const renderFaults = new Set<PageId>()

function sectionFor(page: PageId): HTMLElement | null {
  return pageRoot(page)
}

function renderPage(page: PageId): void {
  const host = sectionFor(page)
  if (!host) return
  try {
    modules[page].render()
    renderFaults.delete(page)
  } catch (err) {
    console.error('[console] page render failed', page, err)
    renderFaults.add(page)
    const lang = state.lang
    clear(host)
    host.append(
      h(
        'div',
        { class: 'errorbox', role: 'alert' },
        h('p', { class: 'error-title' }, tr('pageBrokenTitle', lang)),
        h('p', {}, tr('pageBrokenBody', lang)),
        h('p', { class: 'detail-note' }, tr('pageBrokenDetail', lang)),
        h(
          'div',
          { class: 'chip-row' },
          h(
            'button',
            {
              type: 'button',
              class: 'btn',
              onclick: () => {
                renderPage(page)
              },
            },
            tr('retry', lang),
          ),
          h(
            'button',
            { type: 'button', class: 'btn btn-quiet', onclick: () => window.location.reload() },
            tr('reload', lang),
          ),
        ),
      ),
    )
  }
}

// ---- Frame ------------------------------------------------------------------

function renderNav(): void {
  const nav = document.getElementById('nav')
  if (!nav) return
  const lang = state.lang
  nav.setAttribute('aria-label', tr('navLabel', lang))
  const groups: { label: string; pages: PageId[] }[] = [
    { label: tr('navRunGroup', lang), pages: ['accounts', 'models', 'health'] },
    { label: tr('navDevGroup', lang), pages: ['endpoints', 'integration'] },
  ]
  clear(nav)
  for (const group of groups) {
    const list = h('ul', { class: 'nav-list' })
    for (const page of group.pages) {
      const label = tr(PAGE_LABELS[page], lang)
      const item = h(
        'button',
        {
          type: 'button',
          class: 'nav-item',
          'data-page': page,
          onclick: () => {
            goTo(page)
          },
        },
        h('span', { class: 'nav-index', 'aria-hidden': 'true' }, String(PAGES.indexOf(page) + 1).padStart(2, '0')),
        h('span', { class: 'nav-name' }, label),
      )
      if (page === 'accounts') {
        const summary = accountsSummary()
        item.append(
          h(
            'span',
            { class: 'nav-badge', 'aria-hidden': 'true' },
            summary.count === null ? '·' : String(summary.count),
          ),
        )
      }
      const li = h('li', {}, item)
      list.append(li)
    }
    nav.append(h('div', {}, h('p', { class: 'rail-nav-group' }, group.label), list))
  }
  markCurrent()
}

function markCurrent(): void {
  const nav = document.getElementById('nav')
  if (!nav) return
  for (const button of nav.querySelectorAll<HTMLButtonElement>('.nav-item')) {
    const page = button.dataset.page
    if (page === state.page) button.setAttribute('aria-current', 'page')
    else button.removeAttribute('aria-current')
  }
}

function renderTrail(): void {
  const trail = document.getElementById('trail')
  if (!trail) return
  const lang = state.lang
  clear(trail)
  trail.append(
    h('span', { class: 'crumb' }, tr('crumb', lang)),
    h('span', { 'aria-hidden': 'true' }, '/'),
    h('strong', {}, tr(PAGE_LABELS[state.page], lang)),
  )
  document.title = `${tr(PAGE_LABELS[state.page], lang)} · ${tr('appName', lang)}`
  document.documentElement.lang = lang === 'zh' ? 'zh-CN' : 'en-US'
}

function renderLang(): void {
  const host = document.getElementById('lang')
  if (!host) return
  const lang = state.lang
  host.setAttribute('role', 'group')
  host.setAttribute('aria-label', tr('langGroup', lang))
  clear(host)
  for (const code of ['zh', 'en'] as const) {
    host.append(
      h(
        'button',
        {
          type: 'button',
          class: 'lang-btn',
          lang: code === 'zh' ? 'zh-CN' : 'en-US',
          'aria-pressed': String(lang === code),
          onclick: () => {
            if (lang === code) return
            saveLanguage(code)
            window.location.reload()
          },
        },
        tr(code === 'zh' ? 'langZh' : 'langEn', code),
      ),
    )
  }
}

function renderPort(): void {
  const port = document.getElementById('port')
  if (!port) return
  const lang = state.lang
  port.textContent = `${tr('portLabel', lang)} ${currentPort()}`
}

function tickClock(): void {
  const host = document.getElementById('clock')
  if (!host) return
  const now = new Date()
  const time = new Intl.DateTimeFormat(state.lang === 'zh' ? 'zh-CN' : 'en-US', {
    hour: '2-digit',
    minute: '2-digit',
    second: '2-digit',
    hour12: false,
  }).format(now)
  host.textContent = time
  host.setAttribute('title', tr('clockLabel', state.lang))
}

interface OverviewState {
  alive: boolean | null
  modelCount: number | null
}

const overview: OverviewState = { alive: null, modelCount: null }

function renderOverview(): void {
  const host = document.getElementById('overview')
  if (!host) return
  const lang = state.lang
  const alive = overview.alive
  const tone = alive === null ? 'idle' : alive ? 'ok' : 'bad'
  const label = alive === null ? tr('serviceUnknown', lang) : alive ? tr('serviceRunning', lang) : tr('serviceDown', lang)
  clear(host)
  host.append(
    h('p', { class: 'status-line', 'data-tone': tone }, h('span', { class: 'status-dot', 'aria-hidden': 'true' }), label),
    h(
      'p',
      { class: 'overview-note' },
      alive === true
        ? overview.modelCount === null
          ? tr('modelsCountUnavailable', lang)
          : `${overview.modelCount} ${tr('modelsCount', lang)}`
        : alive === false
          ? tr('serviceDown', lang)
          : tr('serviceUnknown', lang),
    ),
    h('p', { class: 'overview-note' }, `${tr('localAddress', lang)} ${window.location.host || `127.0.0.1:${currentPort()}`}`),
  )
  const sub = document.getElementById('brand-sub')
  if (sub) sub.textContent = tr('overview', lang)
  const foot = document.getElementById('rail-foot')
  if (foot) {
    clear(foot)
    foot.append(h('p', {}, tr('railFootOne', lang)), h('p', {}, tr('railFootTwo', lang)))
  }
}

async function refreshOverview(): Promise<void> {
  overview.alive = await checkLiveness()
  renderOverview()
  try {
    const catalogue = await loadCatalogue()
    overview.modelCount = catalogue.models.length
  } catch {
    overview.modelCount = null
  }
  renderOverview()
}

export function reportCatalogueCount(count: number): void {
  overview.modelCount = count
  renderOverview()
}

// ---- Navigation -------------------------------------------------------------

function showPage(page: PageId): void {
  state.page = page
  for (const candidate of PAGES) {
    const section = sectionFor(candidate)
    if (section) section.hidden = candidate !== page
  }
  markCurrent()
  renderTrail()
}

function goTo(page: PageId, pushHash = true): void {
  if (state.page === page) return
  showPage(page)
  if (pushHash && window.location.hash !== `#${page}`) {
    window.location.hash = page
  }
}

function syncFromHash(): void {
  const page = pageFromHash()
  if (page === state.page) return
  if (!(page in modules)) {
    showPage('accounts')
    return
  }
  showPage(page)
}

// ---- Startup ----------------------------------------------------------------

function start(): void {
  state.lang = detectLanguage()
  try {
    loadStoredPreferences()
  } catch (err) {
    console.error('[console] preferences unavailable', err)
  }
  const shell = document.getElementById('shell')
  if (shell) shell.hidden = false
  const boot = document.getElementById('boot-note')
  if (boot) boot.remove()

  showPage(pageFromHash())
  renderNav()
  renderLang()
  renderPort()
  renderOverview()
  renderTrail()
  for (const candidate of PAGES) {
    const section = sectionFor(candidate)
    if (section) section.hidden = candidate !== state.page
  }

  for (const page of PAGES) {
    renderPage(page)
    try {
      modules[page].attach()
    } catch (err) {
      console.error('[console] page attach failed', page, err)
    }
  }

  try {
    tickClock()
    window.setInterval(tickClock, 1000)
  } catch (err) {
    console.error('[console] clock failed', err)
  }

  window.addEventListener('hashchange', syncFromHash)
  window.addEventListener('error', (event) => {
    console.error('[console] uncaught error', event.error ?? event.message)
    toast(tr('errorUnknown', state.lang), 'bad')
  })
  window.addEventListener('unhandledrejection', (event) => {
    console.error('[console] unhandled rejection', event.reason)
    toast(tr('errorUnknown', state.lang), 'bad')
  })
  document.addEventListener('visibilitychange', () => {
    if (document.visibilityState === 'visible') {
      void refreshOverview()
      void reloadAccounts()
    }
  })
  subscribe(() => {
    renderNav()
  })
  onChange(() => {
    for (const page of PAGES) renderPage(page)
  })

  void refreshOverview()
  void loadCatalogue()
    .then((catalogue) => {
      if (overview.alive !== false) reportCatalogueCount(catalogue.models.length)
    })
    .catch(() => {
      // The catalogue view shows its own error; the rail keeps its placeholder.
    })
  void announce
}

start()
