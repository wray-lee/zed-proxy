// Locale-aware formatting plus the enum-to-copy mappings the pages share.

import type { Lang } from './text.ts'
import { tf, tr } from './text.ts'

const LOCALES: Record<Lang, string> = { zh: 'zh-CN', en: 'en-US' }

export function locale(lang: Lang): string {
  return LOCALES[lang]
}

export function dateTime(seconds: number | null, lang: Lang): string | null {
  if (seconds === null || seconds <= 0) return null
  const date = new Date(seconds * 1000)
  if (Number.isNaN(date.getTime())) return null
  return new Intl.DateTimeFormat(locale(lang), {
    year: 'numeric',
    month: 'short',
    day: 'numeric',
    hour: '2-digit',
    minute: '2-digit',
    hour12: false,
  }).format(date)
}

export function dateOnly(value: string | null, lang: Lang): string | null {
  if (!value) return null
  const date = new Date(value)
  if (Number.isNaN(date.getTime())) return null
  return new Intl.DateTimeFormat(locale(lang), { year: 'numeric', month: 'long', day: 'numeric' }).format(date)
}

export function clock(date: Date, lang: Lang): string {
  return new Intl.DateTimeFormat(locale(lang), {
    hour: '2-digit',
    minute: '2-digit',
    second: '2-digit',
    hour12: false,
  }).format(date)
}

export function money(value: number): string {
  return `$${value.toFixed(2)}`
}

export function fraction(used: number | null, cap: number | null, pct: number | null): number | null {
  if (used !== null && cap !== null && cap > 0) {
    return Math.min(100, Math.max(0, Math.round((used / cap) * 100)))
  }
  if (pct !== null) return Math.min(100, Math.max(0, Math.round(pct)))
  return null
}

/** Strips the machine prefix from a plan name, e.g. `zed_pro` becomes `pro`. */
export function planName(raw: string | null): string | null {
  if (!raw || raw === 'unknown') return null
  const cleaned = raw.replace(/^zed_/, '').replace(/_/g, ' ').trim()
  return cleaned === '' ? null : cleaned
}

export function providerName(ownedBy: string | null): string | null {
  if (ownedBy === null) return null
  if (ownedBy === 'open_ai') return 'OpenAI'
  if (ownedBy === 'anthropic') return 'Anthropic'
  if (ownedBy === 'google') return 'Google'
  return ownedBy
}

export type QuotaTone = 'ok' | 'warn' | 'bad' | 'idle'

export interface QuotaView {
  tone: QuotaTone
  label: string
  detail: string
}

export interface QuotaInput {
  quotaState: string | null
  tokenOk: boolean | null
  checkOk: boolean | null
  used: number | null
  limit: number | null
  remaining: number | null
  usageBased: boolean | null
}

export function quotaView(input: QuotaInput, lang: Lang): QuotaView {
  if (input.tokenOk === false) {
    return { tone: 'bad', label: tr('quotaUnavailable', lang), detail: '' }
  }
  const used = input.used
  const limit = input.limit
  const detailParts: string[] = []
  if (used !== null && limit !== null && limit > 0) {
    detailParts.push(tf('quotaUsed', lang, used, limit))
    if (input.remaining !== null) detailParts.push(tf('quotaRemaining', lang, input.remaining))
  }
  if (input.usageBased) detailParts.push(tr('quotaUsageBased', lang))
  const detail = detailParts.join(' · ')
  switch (input.quotaState) {
    case 'available':
      return { tone: 'ok', label: tr('quotaAvailable', lang), detail }
    case 'exhausted':
      return { tone: 'bad', label: tr('quotaExhausted', lang), detail }
    case 'unmetered':
      return { tone: 'idle', label: tr('quotaUnmetered', lang), detail }
    case 'restricted':
      return { tone: 'warn', label: tr('quotaRestricted', lang), detail }
    case 'unavailable':
      return { tone: 'bad', label: tr('quotaUnavailable', lang), detail }
    case 'unknown':
      return { tone: 'warn', label: tr('quotaUnknown', lang), detail }
    default:
      if (input.checkOk === false) {
        return { tone: 'warn', label: tr('quotaUnknown', lang), detail }
      }
      return { tone: 'idle', label: tr('probeUnchecked', lang), detail }
  }
}

export type ProbeTone = 'ok' | 'warn' | 'bad' | 'idle'

export interface ProbeView {
  tone: ProbeTone
  label: string
}

export function probeView(modelState: string | null, lang: Lang): ProbeView {
  switch (modelState) {
    case 'healthy':
      return { tone: 'ok', label: tr('probeHealthy', lang) }
    case 'auth_error':
      return { tone: 'bad', label: tr('probeAuthError', lang) }
    case 'rate_limited':
      return { tone: 'warn', label: tr('probeRateLimited', lang) }
    case 'upstream_error':
      return { tone: 'bad', label: tr('probeUpstreamError', lang) }
    default:
      return { tone: 'idle', label: tr('probeUnchecked', lang) }
  }
}

/** Only a true flag is announced; an absent or false flag stays silent. */
export function billingNoteCopy(overdue: boolean | null, accountTooYoung: boolean | null, lang: Lang): string | null {
  const parts: string[] = []
  if (overdue === true) parts.push(tr('billingOverdue', lang))
  if (accountTooYoung === true) parts.push(tr('accountTooYoung', lang))
  return parts.length === 0 ? null : parts.join(' ')
}

export function orbErrorCopy(code: string | null, lang: Lang): string {
  switch (code) {
    case 'invalid_link':
      return tr('orbErrInvalidLink', lang)
    case 'timeout':
      return tr('orbErrTimeout', lang)
    case 'parse_error':
      return tr('orbErrParse', lang)
    case 'fetch_failed':
      return tr('orbErrFetch', lang)
    default:
      return tr('orbUnknownError', lang)
  }
}
