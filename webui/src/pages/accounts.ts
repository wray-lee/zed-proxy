// Accounts page: overview metrics, the account list and the login flow.
// Rendering is split into small builders so one failing card cannot blank the page.

import { h, clear, pageRoot } from '../dom.ts'
import { describe, failureFrom } from '../api.ts'
import { completeLogin } from '../data.ts'
import type { AccountRow, StatusRow } from '../data.ts'
import { dateOnly, dateTime, fraction, money, planName, probeView, quotaView, orbErrorCopy } from '../format.ts'
import { confirmDialog } from '../dialog.ts'
import { state } from '../state.ts'
import { tf, tr } from '../text.ts'
import { badge, emptyBlock, errorBlock, iconForTone, loadingBlock, pageHead, section, toast } from '../ui.ts'
import {
  beginLogin,
  ensureCatalogue,
  isFailure,
  probeAccount,
  refreshAllBalances,
  reloadAccounts,
  reloadStatus,
  selectProbeModel,
  storeOrbLink,
  switchTo,
  view,
} from '../store.ts'

const host = (): HTMLElement | null => pageRoot('accounts')

// ---- Login flow -------------------------------------------------------------

interface LoginState {
  open: boolean
  phase: 'idle' | 'starting' | 'waiting' | 'submitting' | 'failed'
  url: string | null
  expiresIn: number | null
  message: string | null
  detail: string | null
  draft: string
}

const login: LoginState = {
  open: false,
  phase: 'idle',
  url: null,
  expiresIn: null,
  message: null,
  detail: null,
  draft: '',
}

// ---- Local UI state ---------------------------------------------------------

const orbDrafts = new Map<string, string>()
const orbErrors = new Map<string, string>()

// ---- Derived data -----------------------------------------------------------

function mergedStatus(row: AccountRow): StatusRow | null {
  return view.status.rows[row.name] ?? null
}

export function accountsSummary(): { count: number | null } {
  return { count: view.accounts.phase === 'ready' ? view.accounts.rows.length : null }
}

function activeName(): string | null {
  if (view.accounts.current) return view.accounts.current
  const current = view.accounts.rows.find((row) => row.current)
  return current ? current.name : null
}

function tokenCounts(): { valid: number; total: number } | null {
  const rows = Object.values(view.status.rows)
  if (rows.length === 0) return null
  let valid = 0
  for (const row of rows) if (row.tokenOk === true) valid += 1
  return { valid, total: rows.length }
}

function probeCounts(): { healthy: number; total: number } | null {
  const rows = Object.values(view.status.rows)
  if (rows.length === 0) return null
  const checked = rows.filter((row) => row.modelState !== null && row.modelState !== 'unchecked')
  if (checked.length === 0) return null
  let healthy = 0
  for (const row of rows) if (row.modelState === 'healthy') healthy += 1
  return { healthy, total: view.accounts.rows.length || rows.length }
}

function cooldownCount(): number {
  let count = 0
  for (const row of Object.values(view.status.rows)) if (row.cooldownS > 0) count += 1
  for (const row of view.accounts.rows) if (!mergedStatus(row) && row.cooldownS > 0) count += 1
  return count
}

function lastCheckedText(): string | null {
  return dateTime(view.status.checkedAt, state.lang)
}

// ---- Metrics ----------------------------------------------------------------

function metric(label: string, value: string, note: string | null): HTMLElement {
  return h(
    'div',
    { class: 'metric' },
    h('p', { class: 'metric-label' }, label),
    h('p', { class: 'metric-value' }, value),
    note ? h('p', { class: 'metric-note' }, note) : null,
  )
}

function metricsBlock(): HTMLElement {
  const lang = state.lang
  const active = activeName()
  const tokens = tokenCounts()
  const probes = probeCounts()
  const checked = lastCheckedText()
  return h(
    'div',
    { class: 'metrics' },
    metric(tr('metricActive', lang), active ?? tr('metricNoActive', lang), null),
    metric(
      tr('metricTokens', lang),
      tokens === null ? tr('placeholder', lang) : `${tokens.valid} / ${tokens.total}`,
      checked ? tf('metricCheckedAt', lang, checked) : tr('metricNeverChecked', lang),
    ),
    metric(
      tr('metricProbes', lang),
      probes === null ? tr('placeholder', lang) : `${probes.healthy} / ${probes.total}`,
      view.probe
        ? `${view.probe.report.model ?? view.probe.model} · ${effortLabel(view.probe.report.effort)}`
        : tr('metricNoProbe', lang),
    ),
    metric(tr('metricCooldowns', lang), String(cooldownCount()), checked ? tf('metricCheckedAt', lang, checked) : tr('metricNeverChecked', lang)),
  )
}

function effortLabel(effort: string | null): string {
  return effort === null ? tr('costProbeNoEffort', state.lang) : effort
}

// ---- Probe budget -----------------------------------------------------------

const GPT55 = /^gpt-5\.5/
const GOOGLE_PROVIDERS = new Set(['google'])

interface Budget {
  tokens: number | null
  /** null means the request carries no output limit. */
  hasLimit: boolean
}

function budgetFor(slug: string): Budget {
  const model = view.catalogue.models.find((row) => row.slug === slug)
  if (model === undefined) return { tokens: 32, hasLimit: true }
  const cap = model.maxOutputTokens !== null && model.maxOutputTokens > 0 ? model.maxOutputTokens : null
  if (GPT55.test(slug)) return { tokens: 32768, hasLimit: true }
  if (model.ownedBy !== null && GOOGLE_PROVIDERS.has(model.ownedBy)) {
    return { tokens: cap !== null ? Math.min(32, cap) : 32, hasLimit: true }
  }
  if (model.ownedBy === 'anthropic') {
    return { tokens: cap !== null ? Math.min(32, cap) : 32, hasLimit: true }
  }
  if (model.ownedBy === 'open_ai' && model.levels.length === 0) {
    return { tokens: cap !== null ? Math.min(1024, cap) : 1024, hasLimit: true }
  }
  return { tokens: cap !== null ? Math.min(32, cap) : 32, hasLimit: true }
}

function costBody(): HTMLElement {
  const lang = state.lang
  const slug = state.probeModel
  const budget = budgetFor(slug)
  const report = view.probe !== null && view.probe.model === slug ? view.probe.report : null
  const parts: Node[] = [
    h('p', { class: 'banner-title' }, tr('costTitle', lang)),
    h('p', {}, tr('costBody', lang)),
  ]
  if (report !== null) {
    if (report.maxOutputTokens === null) parts.push(h('p', {}, tf('costDetailNoLimit', lang, slug)))
    else parts.push(h('p', {}, tf('costDetail', lang, slug, report.maxOutputTokens)))
    parts.push(h('p', {}, tf('costProbeUnknown', lang, effortLabel(report.effort))))
  } else {
    parts.push(h('p', {}, tf('costDetail', lang, slug, budget.tokens ?? 32)))
    const model = view.catalogue.models.find((row) => row.slug === slug)
    parts.push(h('p', {}, tf('costProbeUnknown', lang, effortLabel(model?.defaultEffort ?? null))))
  }
  return h('div', { class: 'banner' }, ...parts)
}

// ---- Account card -----------------------------------------------------------

function statusBadges(status: StatusRow | null): HTMLElement {
  const lang = state.lang
  if (status === null) {
    return h(
      'div',
      { class: 'badge-row' },
      badge(tr('statusNotChecked', lang), 'idle'),
      badge(tr('probeUnchecked', lang), 'idle'),
    )
  }
  const quota = quotaView(
    {
      quotaState: status.quotaState,
      tokenOk: status.tokenOk,
      checkOk: status.checkOk,
      used: status.used,
      limit: status.limit,
      remaining: status.remaining,
      usageBased: status.usageBased,
    },
    lang,
  )
  const probe = probeView(status.modelState, lang)
  return h(
    'div',
    { class: 'badge-row' },
    h('span', { class: 'badge', 'data-tone': quota.tone }, iconForTone(quota.tone), quota.label),
    h('span', { class: 'badge', 'data-tone': probe.tone }, iconForTone(probe.tone), probe.label),
  )
}

function detail(label: string, value: string | null, note: string | null): HTMLElement {
  return h(
    'div',
    { class: 'detail' },
    h('p', { class: 'detail-label' }, label),
    h('p', { class: 'detail-value' }, value ?? tr('notDisclosed', state.lang)),
    note ? h('p', { class: 'detail-note' }, note) : null,
  )
}

function orbBlock(row: AccountRow, status: StatusRow | null): HTMLElement {
  const lang = state.lang
  const name = row.name
  const linked = status?.orbConfigured === true
  const title = h('p', { class: 'orb-title' }, tr('orbTitle', lang))
  const busy = view.busy.orb[name] === true

  if (!linked) {
    const inputId = `orb-${name}`
    const input = h('input', {
      class: 'field-input',
      type: 'url',
      id: inputId,
      value: orbDrafts.get(name) ?? '',
      placeholder: tr('orbLinkPlaceholder', lang),
      autocomplete: 'off',
      spellcheck: 'false',
      autocapitalize: 'off',
      oninput: (event: Event) => {
        const target = event.target
        if (target instanceof HTMLInputElement) {
          orbDrafts.set(name, target.value)
          const err = orbErrors.get(name)
          if (err) {
            orbErrors.delete(name)
          }
        }
      },
      onkeydown: (event) => {
        if (event instanceof KeyboardEvent && event.key === 'Enter') {
          event.preventDefault()
          void submitOrbLink(row.name)
        }
      },
    })
    const error = orbErrors.get(name)
    return h(
      'div',
      { class: 'orb-block' },
      h('div', { class: 'orb-head' }, title, h('span', { class: 'detail-note' }, tr('orbUnlinked', lang))),
      h('p', { class: 'orb-note' }, tr('orbLinkHint', lang)),
      h(
        'div',
        { class: 'field' },
        h('label', { class: 'field-label', for: inputId }, tr('orbLinkLabel', lang)),
        h(
          'div',
          { class: 'field-row' },
          input,
          h(
            'button',
            {
              type: 'button',
              class: 'btn btn-primary',
              'aria-busy': String(busy),
              disabled: busy,
              'aria-label': tf('orbSaveFor', lang, name),
              onclick: () => {
                void submitOrbLink(row.name)
              },
            },
            busy ? tr('orbSaving', lang) : tr('orbSave', lang),
          ),
        ),
        error ? h('p', { class: 'field-error' }, error) : null,
      ),
    )
  }

  const credit = status?.credit ?? null
  if (credit === null) {
    return h(
      'div',
      { class: 'orb-block' },
      h('div', { class: 'orb-head' }, title, h('span', { class: 'detail-note' }, tr('orbReading', lang))),
    )
  }
  if (!credit.ok) {
    return h(
      'div',
      { class: 'orb-block' },
      h('div', { class: 'orb-head' }, title, badge(tr('orbUnknownError', lang), 'bad')),
      h('p', { class: 'orb-note' }, orbErrorCopy(credit.error, lang)),
      unlinkButton(row, busy),
    )
  }
  const pct = fraction(credit.usedUsd ?? null, credit.capUsd ?? null, credit.usedPct)
  const resets = dateOnly(credit.resetsAt, lang)
  return h(
    'div',
    { class: 'orb-block' },
    h('div', { class: 'orb-head' }, title, h('span', { class: 'detail-note' }, tr('orbLinked', lang))),
    h(
      'div',
      { class: 'orb-figures' },
      h('span', {}, tr('orbTitle', lang), ' ', h('strong', {}, money(credit.usedUsd ?? 0)), ' / ', money(credit.capUsd ?? 0)),
      h('span', {}, tr('orbLeft', lang).replace('{0}', money(credit.balanceUsd ?? 0))),
    ),
    pct === null
      ? null
      : h(
          'div',
          { class: 'progress', role: 'progressbar', 'aria-valuemin': '0', 'aria-valuemax': '100', 'aria-valuenow': String(pct), 'aria-label': tf('orbProgress', lang, pct) },
          h('span', { style: `width: ${pct}%` }),
        ),
    pct === null ? null : h('p', { class: 'orb-note' }, tf('orbProgress', lang, pct)),
    resets ? h('p', { class: 'orb-note' }, tf('orbResets', lang, resets)) : null,
    unlinkButton(row, busy),
  )
}

function unlinkButton(row: AccountRow, busy: boolean): HTMLElement {
  const lang = state.lang
  return h(
    'div',
    {},
    h(
      'button',
      {
        type: 'button',
        class: 'btn btn-quiet',
        disabled: busy,
        'aria-label': tf('orbUnlinkFor', lang, row.name),
        onclick: () => {
          confirmDialog(
            tr('orbUnlinkTitle', lang),
            tr('orbUnlinkBody', lang),
            tr('orbUnlinkConfirm', lang),
            tr('cancel', lang),
            tr('dialogClose', lang),
            () => {
              void removeOrbLink(row.name)
            },
          )
        },
      },
      tr('orbUnlink', lang),
    ),
  )
}

function cardActions(row: AccountRow): HTMLElement {
  const lang = state.lang
  const current = activeName() === row.name
  const busyProbe = view.busy.probe[row.name] === true || view.busy.probeAll
  const busySwitch = view.busy.switch[row.name] === true
  return h(
    'div',
    { class: 'card-actions' },
    h(
      'a',
      {
        class: 'btn btn-quiet',
        href: 'https://zed.dev/account',
        target: '_blank',
        rel: 'noopener noreferrer',
        'aria-label': tf('billingFor', lang, row.name),
      },
      tr('billingLink', lang),
    ),
    h(
      'button',
      {
        type: 'button',
        class: 'btn',
        disabled: busyProbe,
        'aria-busy': String(busyProbe),
        'aria-label': tf('probeOneFor', lang, row.name),
        onclick: () => {
          void runSingleProbe(row.name)
        },
      },
      busyProbe ? tf('probeOneBusy', lang, row.name) : tr('probeOne', lang),
    ),
    h('span', { class: 'spacer' }),
    current
      ? h('p', { class: 'static-note' }, tr('currentBadge', lang))
      : h(
          'button',
          {
            type: 'button',
            class: 'btn',
            disabled: busySwitch,
            'aria-busy': String(busySwitch),
            'aria-label': tf('switchToFor', lang, row.name),
            onclick: () => {
              void doSwitch(row.name)
            },
          },
          busySwitch ? tf('switching', lang, row.name) : tr('switchTo', lang),

        ),
  )
}

function accountCard(row: AccountRow): HTMLElement {
  const lang = state.lang
  const status = mergedStatus(row)
  const plan = planName(status?.plan ?? null)
  const ends = dateOnly(status?.subscriptionEndsAt ?? null, lang)
  const probe = status ? probeView(status.modelState, lang) : null
  const probeWhen = status ? dateTime(status.modelCheckedAt, lang) : null
  const probeNote = status
    ? [
        status.modelLatencyMs !== null && status.modelLatencyMs > 0 ? tf('probeLatency', lang, status.modelLatencyMs) : null,
        probeWhen ? tf('probeCheckedAt', lang, probeWhen) : null,
      ]
        .filter((part): part is string => part !== null)
        .join(' · ')
    : null
  const quotaText = status
    ? quotaView(
        {
          quotaState: status.quotaState,
          tokenOk: status.tokenOk,
          checkOk: status.checkOk,
          used: status.used,
          limit: status.limit,
          remaining: status.remaining,
          usageBased: status.usageBased,
        },
        lang,
      )
    : null

  const initial = row.name.slice(0, 1).toUpperCase()
  return h(
    'li',
    { class: 'account-card', 'data-current': String(activeName() === row.name) },
    h(
      'div',
      { class: 'card-head' },
      h('span', { class: 'avatar', 'aria-hidden': 'true' }, initial),
      h(
        'div',
        { class: 'identity' },
        h('p', { class: 'account-name' }, row.name),
        h('p', { class: 'account-meta' }, row.userId ? `${tr('userId', lang)} ${row.userId}` : tr('notDisclosed', lang)),
      ),
      statusBadges(status),
    ),
    h(
      'div',
      { class: 'detail-grid' },
      detail(tr('planLabel', lang), plan ?? tr('planUnknown', lang), status?.billingOk === false ? tr('planBillingUnread', lang) : null),
      detail(tr('periodEnd', lang), ends, null),
      detail(
        tr('quotaLabel', lang),
        quotaText?.detail === '' ? quotaText.label : (quotaText?.detail ?? null),
        status && status.limit === null ? tr('quotaLimitUnknown', lang) : null,
      ),
      detail(
        tr('probeLabel', lang),
        probe?.label ?? tr('statusAwaiting', lang),
        status === null ? tr('statusCardLoading', lang) : probeNote || null,
      ),
    ),
    orbBlock(row, status),
    cardActions(row),
  )
}

// ---- Actions ----------------------------------------------------------------

async function submitOrbLink(account: string): Promise<void> {
  const lang = state.lang
  const raw = (orbDrafts.get(account) ?? '').trim()
  if (raw === '') {
    orbErrors.set(account, tr('orbNeedLink', lang))
    render()
    return
  }
  const failure = await storeOrbLink(account, raw)
  if (failure === null) {
    orbDrafts.delete(account)
    orbErrors.delete(account)
    toast(tf('orbSaved', lang, account), 'ok')
    await Promise.all([reloadAccounts(), reloadStatus()])
  } else {
    orbErrors.set(account, describe(failure, lang))
    toast(`${tr('orbUnknownError', lang)} ${describe(failure, lang)}`, 'bad')
  }
  render()
}

async function removeOrbLink(account: string): Promise<void> {
  const lang = state.lang
  const failure = await storeOrbLink(account, null)
  if (failure === null) {
    toast(tf('orbCleared', lang, account), 'ok')
    await Promise.all([reloadAccounts(), reloadStatus()])
  } else {
    toast(`${tr('orbUnknownError', lang)} ${describe(failure, lang)}`, 'bad')
  }
  render()
}

async function doSwitch(account: string): Promise<void> {
  const lang = state.lang
  const failure = await switchTo(account)
  if (failure === null) {
    toast(tf('switchSuccess', lang, account), 'ok')
  } else {
    toast(`${tr('switchFailed', lang)} ${describe(failure, lang)}`, 'bad')
  }
  render()
}

async function runSingleProbe(account: string): Promise<void> {
  const lang = state.lang
  const result = await probeAccount(account)
  if (isFailure(result)) {
    toast(tr('probeFailed', lang), 'bad')
  } else {
    toast(
      tf('probeDone', lang, result.passed, result.total, result.report.model ?? result.model, effortLabel(result.report.effort)),
      result.passed === result.total ? 'ok' : 'warn',
    )
  }
  render()
}

async function runAllProbes(): Promise<void> {
  const lang = state.lang
  if (view.accounts.rows.length === 0) {
    toast(tr('noAccountsProbe', lang), 'warn')
    return
  }
  const result = await probeAccount(null)
  if (isFailure(result)) {
    toast(tr('probeFailed', lang), 'bad')
  } else {
    toast(
      tf('probeDone', lang, result.passed, result.total, result.report.model ?? result.model, effortLabel(result.report.effort)),
      result.passed === result.total ? 'ok' : 'warn',
    )
  }
  render()
}

async function doRefreshStatus(): Promise<void> {
  const lang = state.lang
  if (view.accounts.rows.length === 0 && view.accounts.phase === 'ready') {
    toast(tr('noAccountsRefresh', lang), 'warn')
    return
  }
  await reloadStatus()
  if (view.status.error !== null && !view.status.stale) {
    toast(tr('statusFailed', lang), 'bad')
  } else if (view.status.error !== null) {
    toast(`${tr('statusFailed', lang)} ${tr('dataFromBefore', lang).replace('{0}', lastCheckedText() ?? '')}`, 'warn')
  } else {
    toast(tr('statusRefreshed', lang), 'ok')
  }
  render()
}

async function doRefreshBalance(): Promise<void> {
  const lang = state.lang
  const linked = Object.values(view.status.rows).some((row) => row.orbConfigured)
  if (!linked) {
    toast(tr('balanceDisabled', lang), 'warn')
    return
  }
  const result = await refreshAllBalances()
  if (isFailure(result)) {
    toast(`${tr('balanceFailed', lang)} ${describe(result, lang)}`, 'bad')
  } else if (result.updated === 0 && result.failed === 0 && result.retryAfterS > 0) {
    toast(tf('balanceWait', lang, result.retryAfterS), 'warn')
  } else if (result.updated === 0 && result.failed === 0) {
    toast(tr('balanceNone', lang), 'warn')
  } else if (result.failed > 0) {
    toast(tf('balancePartial', lang, result.updated, result.failed, orbErrorCopy(result.firstError, lang)), 'warn')
  } else {
    toast(tf('balanceUpdated', lang, result.updated), 'ok')
  }
  render()
}

async function startLoginFlow(): Promise<void> {
  const lang = state.lang
  login.open = true
  login.phase = 'starting'
  login.message = null
  login.detail = null
  render()
  const result = await beginLogin()
  if (isFailure(result)) {
    login.phase = 'failed'
    login.message = describe(result, lang)
  } else if (result.url === null) {
    login.phase = 'failed'
    login.message = tr('loginFailed', lang)
  } else {
    login.phase = 'waiting'
    login.url = result.url
    login.expiresIn = result.expiresIn
  }
  render()
}

async function submitLogin(): Promise<void> {
  const lang = state.lang
  const input = document.getElementById('login-url-input')
  const value = (input instanceof HTMLInputElement ? input.value : login.draft).trim()
  const error = document.getElementById('login-url-error')
  if (value === '') {
    if (error) {
      error.textContent = tr('loginNeedUrl', lang)
      error.hidden = false
    }
    if (input instanceof HTMLInputElement) input.focus()
    return
  }
  login.draft = value
  login.phase = 'submitting'
  if (error) error.hidden = true
  render()
  const field = document.getElementById('login-url-input')
  if (field instanceof HTMLInputElement) field.value = value
  try {
    const name = await completeLogin(value)
    login.open = false
    login.phase = 'idle'
    login.draft = ''
    toast(tf('loginSuccess', lang, name ?? ''), 'ok')
    await Promise.all([reloadAccounts(), reloadStatus()])
  } catch (err) {
    login.phase = 'waiting'
    const failure = failureFrom(err)
    toast(`${tr('loginFailed', lang)} ${describe(failure, lang)}`, 'bad')
  }
  render()
}

// ---- Page -------------------------------------------------------------------

function loginPanel(): HTMLElement | null {
  if (!login.open) return null
  const lang = state.lang
  const children: Node[] = [h('h2', { class: 'section-title' }, tr('loginTitle', lang))]
  if (login.phase === 'starting') {
    children.push(loadingBlock(tr('loginPreparing', lang)))
  } else if (login.phase === 'failed') {
    children.push(
      h(
        'div',
        { class: 'errorbox', role: 'alert' },
        h('p', { class: 'error-title' }, tr('loginFailed', lang)),
        h('p', {}, login.message ?? tr('errorUnknown', lang)),
        h(
          'button',
          { type: 'button', class: 'btn', onclick: () => void startLoginFlow() },
          tr('loginRetry', lang),
        ),
      ),
    )
  } else {
    if (login.url) {
      const minutes = login.expiresIn === null ? null : Math.max(1, Math.round(login.expiresIn / 60))
      children.push(
        h(
          'p',
          {},
          h(
            'a',
            { class: 'login-link', href: login.url, target: '_blank', rel: 'noopener noreferrer' },
            tr('loginOpen', lang),
          ),
        ),
      )
      if (minutes !== null) children.push(h('p', { class: 'field-hint' }, tf('loginExpires', lang, minutes)))
    }
    children.push(
      h(
        'div',
        { class: 'field' },
        h('label', { class: 'field-label', for: 'login-url-input' }, tr('loginPasteLabel', lang)),
        h('p', { class: 'field-hint' }, tr('loginPasteHint', lang)),
        h('input', {
          class: 'field-input',
          id: 'login-url-input',
          type: 'text',
          autocomplete: 'off',
          spellcheck: 'false',
          autocorrect: 'off',
          autocapitalize: 'off',
          placeholder: tr('loginPastePlaceholder', lang),
          value: login.draft,
          disabled: login.phase === 'submitting',
          oninput: (event: Event) => {
            const target = event.target
            if (target instanceof HTMLInputElement) login.draft = target.value
          },
          onkeydown: (event) => {
            if (event instanceof KeyboardEvent && event.key === 'Enter') {
              event.preventDefault()
              void submitLogin()
            }
          },
        }),
        h('p', { class: 'field-error', id: 'login-url-error', hidden: true }),
      ),
    )
    children.push(
      h(
        'div',
        { class: 'chip-row' },
        h(
          'button',
          {
            type: 'button',
            class: 'btn btn-primary',
            disabled: login.phase === 'submitting',
            'aria-busy': String(login.phase === 'submitting'),
            onclick: () => void submitLogin(),
          },
          login.phase === 'submitting' ? tr('loginSubmitting', lang) : tr('loginSubmit', lang),
        ),
        h(
          'button',
          {
            type: 'button',
            class: 'btn btn-quiet',
            onclick: () => {
              login.open = false
              login.phase = 'idle'
              render()
            },
          },
          tr('loginDismiss', lang),
        ),
      ),
    )
  }
  return h('div', { class: 'login-panel' }, ...children)
}

function probeModelControl(): HTMLElement {
  const lang = state.lang
  const models = view.catalogue.models
  const options: Node[] = []
  const seen = new Set<string>()
  for (const row of models) {
    seen.add(row.slug)
    options.push(h('option', { value: row.slug, selected: row.slug === state.probeModel }, row.displayName))
  }
  if (!seen.has(state.probeModel)) {
    options.push(h('option', { value: state.probeModel, selected: true }, state.probeModel))
  }
  return h(
    'div',
    { class: 'field' },
    h('label', { class: 'field-label', for: 'probe-model' }, tr('probeModelLabel', lang)),
    h(
      'select',
      {
        class: 'select',
        id: 'probe-model',
        disabled: view.busy.probeAll,
        onchange: (event: Event) => {
          const target = event.target
          if (!(target instanceof HTMLSelectElement)) return
          selectProbeModel(target.value)
        },
      },
      ...options,
    ),
  )
}

function headerActions(): HTMLElement[] {
  const lang = state.lang
  const noAccounts = view.accounts.phase === 'ready' && view.accounts.rows.length === 0
  const linked = Object.values(view.status.rows).some((row) => row.orbConfigured)
  return [
    probeModelControl(),
    h(
      'button',
      {
        type: 'button',
        class: 'btn',
        disabled: view.busy.status || view.busy.probeAll,
        'aria-busy': String(view.busy.status),
        onclick: () => void doRefreshStatus(),
      },
      view.busy.status ? tr('refreshStatusBusy', lang) : tr('refreshStatus', lang),
    ),
    h(
      'button',
      {
        type: 'button',
        class: 'btn',
        disabled: view.busy.balance || view.busy.status || !linked || noAccounts,
        'aria-busy': String(view.busy.balance),
        title: linked ? undefined : tr('balanceDisabled', lang),
        onclick: () => void doRefreshBalance(),
      },
      view.busy.balance ? tr('refreshBalanceBusy', lang) : tr('refreshBalance', lang),
    ),
    h(
      'button',
      {
        type: 'button',
        class: 'btn btn-primary',
        disabled: view.busy.probeAll || view.busy.status || noAccounts,
        'aria-busy': String(view.busy.probeAll),
        onclick: () => void runAllProbes(),
      },
      view.busy.probeAll ? tr('probeAllBusy', lang) : tr('probeAll', lang),
    ),
  ]
}

function accountsBody(): HTMLElement {
  const lang = state.lang
  const phase = view.accounts.phase
  if (phase === 'error' && view.accounts.rows.length === 0) {
    const failure = view.accounts.error ?? failureFrom(new Error('unknown'))
    return errorBlock({
      titleKey: null,
      title: tf('loadFailedTitle', lang, tr('loadFailedTitleAccounts', lang)),
      failure,
      onRetry: () => void reloadAccounts(),
    })
  }
  if (phase === 'loading' && view.accounts.rows.length === 0) {
    return loadingBlock(tr('loadingDetail', lang))
  }
  if (phase === 'ready' && view.accounts.rows.length === 0) {
    return emptyBlock(tr('accountsEmpty', lang))
  }
  const list = h('ul', { class: 'account-list' })
  for (const row of view.accounts.rows) {
    try {
      list.append(accountCard(row))
    } catch (err) {
      console.error('[console] account card failed', row.name, err)
      list.append(
        h(
          'li',
          { class: 'account-card' },
          h('div', { class: 'errorbox', role: 'alert' }, h('p', { class: 'error-title' }, tr('pageBrokenTitle', lang))),
        ),
      )
    }
  }
  return list
}

function render(): void {
  const node = host()
  if (!node) return
  const lang = state.lang
  clear(node)
  node.append(
    pageHead(
      'accounts-title',
      tr('accountsTitle', lang),
      tr('accountsLede', lang),
      headerActions(),
    ),
    costBody(),
    metricsBlock(),
    section(tr('accountsHeading', lang), tr('accountsLedeShort', lang), accountsBody()),
    h(
      'div',
      { class: 'section' },
      h(
        'button',
        {
          type: 'button',
          class: 'btn btn-primary',
          disabled: login.open && login.phase === 'starting',
          onclick: () => void startLoginFlow(),
        },
        tr('addAccount', lang),
      ),
      h('p', { class: 'field-hint' }, tr('addAccountNote', lang)),
      loginPanel(),
    ),
  )
}

let attached = false

export const attachAccountsPage = {
  render,
  attach(): void {
    ensureCatalogue()
    if (!attached) {
      attached = true
      void reloadAccounts()
      void reloadStatus()
    }
  },
}

