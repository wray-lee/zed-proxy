// Boot order: framework first, preferences second (tolerantly), pages third,
// global listeners last. Nothing before the framework may throw uncaught.

import './theme.css'
import { clear, h, pageRoot } from './dom.ts'
import {
  announce,
  currentPort,
  detectLanguage,
  loadStoredPreferences,
  pageFromHash,
  PAGES,
  saveLanguage,
  state,
  subscribe,
} from './state.ts'
import type { PageId } from './state.ts'
import type { TextKey } from './text.ts'
import { tr } from './text.ts'
import { checkLiveness, loadCatalogue } from './data.ts'
import { toast } from './ui.ts'
import { attachAccountsPage, accountsSummary } from './pages/accounts.ts'
import { attachModelsPage } from './pages/models.ts'
import { attachHealthPage } from './pages/health.ts'
import { attachRoutesPage } from './pages/routes.ts'
import { attachClientsPage } from './pages/clients.ts'
import { onChange, view } from './store.ts'

interface PageModule {
  render(): void
  attach(): void
}

const modules: Record<PageId, PageModule> = {
  accounts: attachAccountsPage,
  models: attachModelsPage,
  health: attachHealthPage,
  endpoints: attachRoutesPage,
  integration: attachClientsPage,
}

const PAGE_LABELS: Record<PageId, TextKey> = {
  accounts: 'navAccounts',
  models: 'navModels',
  health: 'navDiagnostics',
  endpoints: 'navRoutes',
  integration: 'navConnect',
}

function sectionFor(page: PageId): HTMLElement | null {
  return pageRoot(page)
}

function focusKey(node: Element | null): string | null {
  if (!(node instanceof HTMLElement)) return null
  if (node.id) return `#${CSS.escape(node.id)}`
  const label = node.getAttribute('aria-label')
  if (label) return `[aria-label="${CSS.escape(label)}"]`
  return null
}

/** Pages whose last render threw stay on the error surface until retried. */
const faultedPages = new Set<PageId>()

function renderPage(page: PageId): void {
  const host = sectionFor(page)
  if (!host) return
  if (faultedPages.has(page)) {
    renderPageFault(host, page)
    return
  }
  const active = document.activeElement
  const restore = host.contains(active) ? focusKey(active) : null
  try {
    modules[page].render()
  } catch (err) {
    console.error('[console] page render failed', page, err)
    faultedPages.add(page)
    renderPageFault(host, page)
    return
  }
  if (restore) {
    const target = host.querySelector(restore)
    if (target instanceof HTMLElement) target.focus()
  }
}

function renderPageFault(host: HTMLElement, page: PageId): void {
  const lang = state.lang
  try {
    drawPageFault(host, page, lang)
  } catch (err) {
    // The error surface must not depend on the DOM helpers that just failed.
    console.error('[console] error surface failed', err)
    host.textContent = tr('pageBrokenTitle', lang)
  }
}

function drawPageFault(host: HTMLElement, page: PageId, lang: 'zh' | 'en'): void {
  clear(host)
  host.append(
    h(
      'div',
      { class: 'errorbox', role: 'alert' },
      h('h1', { class: 'error-title', id: `${page}-title` }, tr('pageBrokenTitle', lang)),
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
              faultedPages.delete(page)
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

// ---- Frame ------------------------------------------------------------------

function renderNav(): void {
  const nav = document.getElementById('nav')
  if (!nav) return
  const lang = state.lang
  nav.setAttribute('aria-label', tr('navLabel', lang))
  const groups: { label: TextKey; pages: PageId[] }[] = [
    { label: 'navRunGroup', pages: ['accounts', 'models', 'health'] },
    { label: 'navDevGroup', pages: ['endpoints', 'integration'] },
  ]
  clear(nav)
  for (const group of groups) {
    const list = h('ul', { class: 'nav-list' })
    for (const page of group.pages) {
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
        h('span', { class: 'nav-name' }, tr(PAGE_LABELS[page], lang)),
      )
      list.append(h('li', {}, item))
    }
    nav.append(h('div', {}, h('p', { class: 'rail-nav-group' }, tr(group.label, lang)), list))
  }
  markCurrent()
}

/** Updates only the account-count badge so a nav button keeps keyboard focus. */
function renderNavBadge(): void {
  const nav = document.getElementById('nav')
  if (!nav) return
  const item = nav.querySelector<HTMLButtonElement>('.nav-item[data-page="accounts"]')
  if (!item) return
  const { count } = accountsSummary()
  const existing = item.querySelector('.nav-badge')
  if (count === null) {
    existing?.remove()
    return
  }
  const label = String(count)
  if (existing) {
    if (existing.textContent !== label) existing.textContent = label
    return
  }
  item.append(h('span', { class: 'nav-badge', 'aria-hidden': 'true' }, label))
}

function markCurrent(): void {
  const nav = document.getElementById('nav')
  if (!nav) return
  for (const button of nav.querySelectorAll<HTMLButtonElement>('.nav-item')) {
    if (button.dataset.page === state.page) button.setAttribute('aria-current', 'page')
    else button.removeAttribute('aria-current')
  }
}

function renderTrail(): void {
  const trail = document.getElementById('trail')
  if (!trail) return
  const lang = state.lang
  clear(trail)
  trail.append(
    h('span', {}, tr('crumb', lang)),
    h('span', { 'aria-hidden': 'true' }, '/'),
    h('strong', {}, tr(PAGE_LABELS[state.page], lang)),
  )
  document.title = `${tr(PAGE_LABELS[state.page], lang)} · ${tr('appName', lang)}`
}

function renderLanguage(): void {
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

function renderChrome(): void {
  const lang = state.lang
  document.documentElement.lang = lang === 'zh' ? 'zh-CN' : 'en-US'
  const skip = document.getElementById('skip-link')
  if (skip) skip.textContent = tr('skipLink', lang)
  const rail = document.getElementById('rail')
  if (rail) rail.setAttribute('aria-label', tr('overview', lang))
  const sub = document.getElementById('brand-sub')
  if (sub) sub.textContent = tr('overview', lang)
  renderNav()
  renderLanguage()
  renderTrail()
  const port = document.getElementById('port')
  if (port) port.textContent = `${tr('portLabel', lang)} ${currentPort()}`
  renderOverview()
  const foot = document.getElementById('rail-foot')
  if (foot) {
    clear(foot)
    foot.append(h('p', {}, tr('railFootOne', lang)), h('p', {}, tr('railFootTwo', lang)))
  }
}

function renderOverview(): void {
  const host = document.getElementById('overview')
  if (!host) return
  const lang = state.lang
  const { alive, modelCount } = view.overview
  const tone = alive === null ? 'idle' : alive ? 'ok' : 'bad'
  const label = alive === null ? tr('serviceUnknown', lang) : alive ? tr('serviceRunning', lang) : tr('serviceDown', lang)
  const modelLine =
    alive === true
      ? modelCount === null
        ? tr('modelsCountUnavailable', lang)
        : `${modelCount} ${tr('modelsCount', lang)}`
      : alive === false
        ? tr('serviceDown', lang)
        : tr('serviceUnknown', lang)
  clear(host)
  host.append(
    h(
      'p',
      { class: 'status-line', 'data-tone': tone },
      h('span', { class: 'status-dot', 'aria-hidden': 'true' }),
      label,
    ),
    h('p', { class: 'overview-note' }, modelLine),
    h(
      'p',
      { class: 'overview-note' },
      `${tr('localAddress', lang)} ${window.location.host || `127.0.0.1:${currentPort()}`}`,
    ),
  )
}

function tickClock(): void {
  const host = document.getElementById('clock')
  if (!host) return
  host.textContent = new Intl.DateTimeFormat(state.lang === 'zh' ? 'zh-CN' : 'en-US', {
    hour: '2-digit',
    minute: '2-digit',
    second: '2-digit',
    hour12: false,
  }).format(new Date())
  host.setAttribute('title', tr('clockLabel', state.lang))
}

async function refreshOverview(): Promise<void> {
  const alive = await checkLiveness()
  view.overview.alive = alive
  renderOverview()
  if (!alive) return
  try {
    const catalogue = await loadCatalogue()
    view.overview.modelCount = catalogue.models.length
  } catch {
    view.overview.modelCount = null
  }
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

function goTo(page: PageId): void {
  if (state.page === page) return
  showPage(page)
  renderPage(page)
  if (window.location.hash !== `#${page}`) window.location.hash = page
}

function syncFromHash(): void {
  const page = pageFromHash()
  if (page === state.page) return
  showPage(page)
  renderPage(page)
  announce()
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
  renderChrome()
  tickClock()

  // Store changes only redraw the visible page; the others redraw when shown.
  const renderCurrentPage = (): void => renderPage(state.page)

  for (const page of PAGES) {
    renderPage(page)
    try {
      modules[page].attach()
    } catch (err) {
      console.error('[console] page attach failed', page, err)
    }
  }

  window.setInterval(tickClock, 1000)
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
    if (document.visibilityState === 'visible') void refreshOverview()
  })
  renderNavBadge()
  subscribe(renderChrome)
  onChange(() => {
    renderNavBadge()
    renderCurrentPage()
  })

  void refreshOverview()
}

start()
