export type Lang = 'zh' | 'en'

const STORAGE_KEY = 'zed_lang'

function detect(): Lang {
  const saved = localStorage.getItem(STORAGE_KEY)
  if (saved === 'zh' || saved === 'en') return saved
  return navigator.language.toLowerCase().startsWith('zh') ? 'zh' : 'en'
}

export const lang: Lang = detect()
/** BCP 47 tag for Intl / toLocale*String. */
export const locale = lang === 'zh' ? 'zh-CN' : 'en-US'

document.documentElement.lang = locale

/** Pick the string for the active language; both are always evaluated. */
export function t(zh: string, en: string): string {
  return lang === 'zh' ? zh : en
}

/** Persist the choice and reload, so every page re-renders in the new language. */
export function setLang(next: Lang): void {
  localStorage.setItem(STORAGE_KEY, next)
  location.reload()
}
