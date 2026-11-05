import { t } from '../i18n'
import { icons } from '../icons'
import { catalogModels, getProbeModel, loadCatalog, onProbeModelChange, resolveProbeModel, setProbeModel } from '../probeModel'
import { showToast } from '../toast'
import type { ModelInfo } from '../api'

const VENDORS: Record<string, string> = {
  open_ai: 'OpenAI',
  anthropic: 'Anthropic',
  google: 'Google',
}

let models: ModelInfo[] = []
let vendors = new Map<string, string>()

function escapeHtml(value: string): string {
  const element = document.createElement('div')
  element.textContent = value
  return element.innerHTML
}

function vendorLabel(slug: string): string {
  const raw = vendors.get(slug)
  if (!raw) return t('未知', 'Unknown')
  return VENDORS[raw] ?? raw
}

function renderRows() {
  const list = document.getElementById('model-list')
  if (!list) return
  if (models.length === 0) {
    list.innerHTML = `<div class="empty-state"><strong>${t('没有可用模型', 'No models available')}</strong><p>${t('网关当前没有返回模型列表。', 'The gateway did not return a model list.')}</p></div>`
    return
  }

  const active = getProbeModel()
  list.innerHTML = models.map(model => {
    const levels = model.supported_reasoning_levels?.map(level => level.effort) ?? []
    const current = model.slug === active
    return `
      <article class="model-row${current ? ' selected' : ''}">
        <div class="model-copy">
          <div class="model-title">
            <strong>${escapeHtml(model.display_name)}</strong>
            <code>${escapeHtml(model.slug)}</code>
            <span class="vendor-tag">${escapeHtml(vendorLabel(model.slug))}</span>
            ${current ? `<span class="state-pill available">${t('当前实测模型', 'Live check model')}</span>` : ''}
          </div>
          ${model.description ? `<p>${escapeHtml(model.description)}</p>` : ''}
          <div class="model-levels">
            <span>${t('默认档位', 'Default level')} <strong>${escapeHtml(model.default_reasoning_level ?? '--')}</strong></span>
            <span class="level-list">${t('支持档位', 'Supported levels')}${levels.length
              ? levels.map(level => `<span class="level-chip">${escapeHtml(level)}</span>`).join('')
              : ' <strong>--</strong>'}</span>
          </div>
        </div>
        <button class="button ${current ? 'secondary' : 'primary'} model-use-button" data-slug="${escapeHtml(model.slug)}" type="button" ${current ? 'disabled' : ''}>
          ${current ? `${icons.check} ${t('已选用', 'Selected')}` : `${icons.activity} ${t('用于实测', 'Use for live check')}`}
        </button>
      </article>
    `
  }).join('')

  list.querySelectorAll<HTMLButtonElement>('.model-use-button').forEach(button => {
    button.addEventListener('click', () => {
      const slug = button.dataset.slug
      if (!slug || slug === getProbeModel()) return
      setProbeModel(slug)
      const name = models.find(model => model.slug === slug)?.display_name ?? slug
      showToast(t(`实测将使用 ${name}`, `Live checks will use ${name}`))
    })
  })
}

async function loadModelPage() {
  const list = document.getElementById('model-list')!
  try {
    const data = await loadCatalog()
    vendors = new Map((data.data ?? []).map(entry => [entry.id, entry.owned_by ?? '']))
    models = catalogModels(data)
    resolveProbeModel(models)
    renderRows()
  } catch (error) {
    const message = escapeHtml(error instanceof Error ? error.message : t('未知错误', 'Unknown error'))
    list.innerHTML = `<div class="error-state">${t(`模型列表加载失败：${message}`, `Failed to load the model list: ${message}`)}</div>`
  }
}

export function renderModels() {
  const page = document.getElementById('page-models')!
  page.innerHTML = `
    <div class="page-heading">
      <div>
        <span class="eyebrow">${t('模型目录', 'Model catalog')}</span>
        <h1>${t('模型', 'Models')}</h1>
        <p>${t('查看网关转发的全部模型及其推理档位。这里的选用只影响账号中心的实测请求，不会改变客户端可用的模型。', 'Review every model the gateway proxies and its reasoning levels. The selection here only changes live-check requests in Account center; it does not restrict the models clients can use.')}</p>
      </div>
    </div>

    <div class="notice-banner neutral">
      <span>${icons.activity}</span>
      <p>${t('“用于实测”会把该模型保存为账号中心实测请求的模型；令牌、套餐和额度检查仍然不会调用模型。', '"Use for live check" saves the model for Account center live checks; token, plan and quota checks still never call a model.')}</p>
    </div>

    <div class="model-list" id="model-list">
      <div class="loading-panel"><span class="spinner"></span>${t('正在加载模型', 'Loading models')}</div>
    </div>
  `

  onProbeModelChange(() => renderRows())
  void loadModelPage()
}
