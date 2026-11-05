export type Lang = 'zh' | 'en'

const STORAGE_KEY = 'zed_lang'

/**
 * 浏览器禁用站点数据时访问 localStorage 会抛 SecurityError。这里必须降级到
 * 默认语言，否则模块初始化就失败，整个控制台只剩空白页。
 */
function storedLang(): string | null {
  try {
    return localStorage.getItem(STORAGE_KEY)
  } catch {
    return null
  }
}

function detect(): Lang {
  const saved = storedLang()
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
  try {
    localStorage.setItem(STORAGE_KEY, next)
  } catch {
    // 存储被禁用时仍切到该语言，只是刷新后不记得。
  }
  location.reload()
}
