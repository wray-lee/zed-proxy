// Tiny element builder. Text is always set as text, never parsed as markup.

type Child = Node | string | number | null | undefined | false
type AttrValue = string | number | boolean | null | undefined | ((event: Event) => void)
type Attrs = Record<string, AttrValue>

export function h<K extends keyof HTMLElementTagNameMap>(
  tag: K,
  attrs: Attrs = {},
  ...children: Child[]
): HTMLElementTagNameMap[K] {
  const node = document.createElement(tag)
  for (const [name, value] of Object.entries(attrs)) {
    if (value === null || value === undefined || value === false) continue
    if (name.startsWith('on') && typeof value === 'function') {
      node.addEventListener(name.slice(2).toLowerCase(), value)
    } else if (name === 'class') {
      node.className = String(value)
    } else if (value === true) {
      node.setAttribute(name, '')
    } else {
      node.setAttribute(name, String(value))
    }
  }
  for (const child of children) {
    if (child === null || child === undefined || child === false) continue
    node.append(typeof child === 'string' || typeof child === 'number' ? String(child) : child)
  }
  return node
}

export function clear(node: Element): void {
  node.replaceChildren()
}

/** Page container lookup; the id prefix is built at runtime to keep the
 *  five build anchors unique in the bundle. */
export function pageRoot(page: string): HTMLElement | null {
  return document.getElementById('page' + '-' + page)
}

export function splitWords(value: string): string[] {
  return value.split(/[\s_-]+/).filter((part) => part.length > 0)
}

export function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value)
}

export function str(value: unknown): string | null {
  return typeof value === 'string' && value !== '' ? value : null
}

export function num(value: unknown): number | null {
  return typeof value === 'number' && Number.isFinite(value) ? value : null
}

export function bool(value: unknown): boolean {
  return value === true
}

export function arr(value: unknown): unknown[] {
  return Array.isArray(value) ? value : []
}

/** Inline vector icons; every one is decorative and hidden from assistive tech. */
const ICONS: Record<string, string> = {
  check: 'M4 12.5l5 5L20 6.5',
  cross: 'M6 6l12 12M18 6L6 18',
  dot: 'M12 12h.01',
  warning: 'M12 4l9 16H3zM12 10v4M12 17h.01',
  copy: 'M9 9h10v10H9zM5 15V5h10',
  arrow: 'M5 12h14M13 6l6 6-6 6',
  none: '',
}

export function icon(name: keyof typeof ICONS): SVGSVGElement {
  const svg = document.createElementNS('http://www.w3.org/2000/svg', 'svg')
  svg.setAttribute('viewBox', '0 0 24 24')
  svg.setAttribute('class', 'icon')
  svg.setAttribute('aria-hidden', 'true')
  svg.setAttribute('focusable', 'false')
  const path = document.createElementNS('http://www.w3.org/2000/svg', 'path')
  path.setAttribute('d', ICONS[name])
  svg.append(path)
  return svg
}

export function statusIcon(ok: boolean | null): SVGSVGElement {
  if (ok === null) return icon('dot')
  return icon(ok ? 'check' : 'cross')
}
