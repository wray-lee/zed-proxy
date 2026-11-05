import { fetchModels, type ModelInfo, type ModelsResponse } from './api'

// 实测模型的选择在账号中心和模型列表之间共享，因此单独存放并广播变更。
const STORAGE_KEY = 'zed_probe_model'
const CHANGE_EVENT = 'zed-probe-model-change'

export const DEFAULT_PROBE_MODEL = 'gpt-5.6-luna'

export function getProbeModel(): string {
  return localStorage.getItem(STORAGE_KEY) || DEFAULT_PROBE_MODEL
}

export function setProbeModel(slug: string): void {
  localStorage.setItem(STORAGE_KEY, slug)
  window.dispatchEvent(new Event(CHANGE_EVENT))
}

export function onProbeModelChange(handler: () => void): void {
  window.addEventListener(CHANGE_EVENT, handler)
}

/** /v1/models 一次请求内复用，两个页面都会读取同一份目录。 */
let catalog: Promise<ModelsResponse> | undefined

export function loadCatalog(): Promise<ModelsResponse> {
  catalog ??= fetchModels().catch(error => {
    catalog = undefined
    throw error
  })
  return catalog
}

/**
 * `/v1/models` returns both shapes: `data` with ids from the upstream list and
 * `models` with the local catalog. Union them so a model that only the upstream
 * advertises (and the probe accepts) still shows up and stays selectable.
 */
export function catalogModels(data: ModelsResponse): ModelInfo[] {
  const listed = data.models ?? []
  const seen = new Set(listed.map(model => model.slug))
  const extra = (data.data ?? [])
    .filter(entry => !seen.has(entry.id))
    .map(entry => ({ slug: entry.id, display_name: entry.id }))
  return listed.concat(extra)
}

/** 保存的模型若已从列表中移除，回落到默认模型或第一项。 */
export function resolveProbeModel(models: ModelInfo[]): string {
  const slugs = models.map(model => model.slug)
  const saved = getProbeModel()
  const picked = slugs.includes(saved)
    ? saved
    : slugs.includes(DEFAULT_PROBE_MODEL) ? DEFAULT_PROBE_MODEL : slugs[0] ?? DEFAULT_PROBE_MODEL
  if (picked !== saved) localStorage.setItem(STORAGE_KEY, picked)
  return picked
}

/**
 * 用模型目录填充账号中心的实测模型下拉框。目录还没到时只放当前保存的项，
 * 不能调用 resolveProbeModel，否则会把用户之前的选择覆盖成默认模型。
 */
export function renderProbeModelOptions(select: HTMLSelectElement, models: ModelInfo[]): void {
  if (models.length === 0) {
    select.replaceChildren(new Option(getProbeModel(), getProbeModel(), true, true))
    return
  }
  const picked = resolveProbeModel(models)
  select.replaceChildren(...models.map(model => new Option(model.display_name, model.slug, false, model.slug === picked)))
}
