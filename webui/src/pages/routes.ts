// Routes page: a static list of every route the gateway exposes. No requests.

import { h, clear, pageRoot } from '../dom.ts'
import type { TextKey } from '../text.ts'
import { tr } from '../text.ts'
import { state } from '../state.ts'
import { pageHead } from '../ui.ts'

interface RouteRow {
  method: string
  path: string
  purpose: TextKey
  key: boolean
}

interface RouteGroup {
  title: TextKey
  rows: RouteRow[]
}

const GROUPS: RouteGroup[] = [
  {
    title: 'routesGroupCore',
    rows: [
      { method: 'GET', path: '/', purpose: 'routesPurposeRoot', key: false },
      { method: 'GET', path: '/health', purpose: 'routesPurposeLiveness', key: false },
      { method: 'POST', path: '/api/event_logging/batch', purpose: 'routesPurposeEvents', key: false },
    ],
  },
  {
    title: 'routesGroupModels',
    rows: [
      { method: 'GET', path: '/v1/models', purpose: 'routesPurposeModels', key: true },
      { method: 'POST', path: '/v1/chat/completions', purpose: 'routesPurposeChat', key: true },
      { method: 'POST', path: '/v1/responses', purpose: 'routesPurposeResponses', key: true },
      { method: 'POST', path: '/v1/messages', purpose: 'routesPurposeMessages', key: true },
    ],
  },
  {
    title: 'routesGroupCompat',
    rows: [
      { method: 'POST', path: '/v1/messages/count_tokens…', purpose: 'routesPurposeCount', key: true },
      { method: 'OPTIONS', path: '*', purpose: 'routesPurposeOptions', key: false },
    ],
  },
  {
    title: 'routesGroupAccounts',
    rows: [
      { method: 'GET', path: '/zed/accounts', purpose: 'routesPurposeAccounts', key: true },
      { method: 'GET', path: '/zed/accounts/status', purpose: 'routesPurposeStatus', key: true },
      { method: 'POST', path: '/zed/accounts/health', purpose: 'routesPurposeProbe', key: true },
      { method: 'POST', path: '/zed/accounts/switch', purpose: 'routesPurposeSwitch', key: true },
      { method: 'POST', path: '/zed/accounts/orb', purpose: 'routesPurposeOrb', key: true },
      { method: 'POST', path: '/zed/accounts/orb/refresh', purpose: 'routesPurposeOrbRefresh', key: true },
    ],
  },
  {
    title: 'routesGroupCatalogue',
    rows: [
      { method: 'POST', path: '/zed/models/refresh', purpose: 'routesPurposeModelsRefresh', key: true },
      { method: 'GET', path: '/zed/usage', purpose: 'routesPurposeUsage', key: true },
      { method: 'GET', path: '/zed/billing', purpose: 'routesPurposeBilling', key: true },
    ],
  },
  {
    title: 'routesGroupLogin',
    rows: [
      { method: 'POST', path: '/zed/login', purpose: 'routesPurposeLogin', key: true },
      { method: 'POST', path: '/zed/login/complete', purpose: 'routesPurposeLoginComplete', key: true },
      { method: 'GET', path: '/zed/login/status', purpose: 'routesPurposeLoginStatus', key: true },
    ],
  },
]

const PROTOCOLS: { name: string; path: string; client: string }[] = [
  { name: 'routesProtocolChat', path: '/v1/chat/completions', client: 'OpenCode, OpenAI SDK' },
  { name: 'routesProtocolResponses', path: '/v1/responses', client: 'Codex' },
  { name: 'routesProtocolMessages', path: '/v1/messages', client: 'Claude Code' },
]

function routeRow(row: RouteRow): HTMLElement {
  const lang = state.lang
  return h(
    'li',
    { class: 'route-row' },
    h('span', { class: 'method' }, row.method),
    h('span', { class: 'route-path' }, row.path),
    h('span', { class: 'route-purpose' }, tr(row.purpose, lang)),
    h('span', { class: 'key-flag', 'data-key': row.key ? 'yes' : 'no' }, row.key ? tr('routesKeyYes', lang) : tr('routesKeyNo', lang)),
  )
}

function render(): void {
  const node = pageRoot('endpoints')
  if (!node) return
  const lang = state.lang
  clear(node)
  const strip = h('div', { class: 'protocol-strip' })
  for (const protocol of PROTOCOLS) {
    strip.append(
      h(
        'div',
        { class: 'protocol' },
        h('p', { class: 'protocol-name' }, tr(protocol.name as TextKey, lang)),
        h('p', { class: 'protocol-path' }, protocol.path),
        h('p', { class: 'section-note' }, protocol.client),
      ),
    )
  }
  node.append(
    pageHead('endpoints-title', tr('routesTitle', lang), tr('routesLede', lang)),
    strip,
  )
  for (const group of GROUPS) {
    const list = h('ul', { class: 'route-table' })
    for (const row of group.rows) list.append(routeRow(row))
    node.append(
      h(
        'section',
        { class: 'route-group' },
        h('h2', { class: 'section-title' }, tr(group.title, lang)),
        list,
      ),
    )
  }
  node.append(h('p', { class: 'section-note' }, tr('routesOfflineNote', lang)))
}

export const attachRoutesPage = {
  render,
  attach(): void {
    // Static page: nothing to load.
  },
}
