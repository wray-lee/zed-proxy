import { t } from '../i18n'
import { icons } from '../icons'
import { catalogModels, getProbeModel, loadCatalog, onProbeModelChange, reloadCatalog, resolveProbeModel, setProbeModel } from '../probeModel'
import { showToast } from '../toast'
import { refreshModels, type ModelInfo } from '../api'

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

async function loadModelPage(loader = loadCatalog) {
  const list = document.getElementById('model-list')!
  try {
    const data = await loader()
    vendors = new Map((data.data ?? []).map(entry => [entry.id, entry.owned_by ?? '']))
    models = catalogModels(data)
    resolveProbeModel(models)
    renderRows()
    updateModelCount((data.data ?? []).length)
  } catch (error) {
    const message = escapeHtml(error instanceof Error ? error.message : t('未知错误', 'Unknown error'))
    list.innerHTML = `<div class="error-state">${t(`模型列表加载失败：${message}`, `Failed to load the model list: ${message}`)}</div>`
  }
}

/** 侧栏的“N 个模型”跟着刷新结果走，不必等下一次可见性检查。 */
function updateModelCount(count: number) {
  const label = document.getElementById('model-count')
  if (label) label.textContent = t(`${count} 个模型`, `${count} ${count === 1 ? 'model' : 'models'}`)
}

async function refreshModelList(button: HTMLButtonElement) {
  if (button.disabled) return
  button.disabled = true
  const idle = `${icons.refresh} ${t('获取最新模型列表', 'Refresh model list')}`
  button.innerHTML = `<span class="spinner"></span>${t('正在刷新', 'Refreshing')}`
  try {
    const result = await refreshModels()
    if (result.refreshed) {
      await loadModelPage(reloadCatalog)
      showToast(t(`已更新 ${result.count ?? 0} 个模型`, `Updated ${result.count ?? 0} models`))
    } else if (result.retry_after_s) {
      showToast(t(`操作太频繁，${result.retry_after_s} 秒后可再次刷新`, `Too many requests; try again in ${result.retry_after_s}s`))
    } else {
      // 无账号时后端返回内置目录，列表本身没变。
      showToast(t(`没有可用账号，继续使用内置目录（${result.count ?? 0} 个模型）`, `No accounts available; still using the built-in catalog (${result.count ?? 0} models)`))
    }
  } catch (error) {
    // 失败时保留旧列表，只提示错误码。
    const message = error instanceof Error ? error.message : t('未知错误', 'Unknown error')
    showToast(t(`模型刷新失败：${message}`, `Failed to refresh models: ${message}`))
  } finally {
    button.disabled = false
    button.innerHTML = idle
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
      <div class="heading-actions">
        <button class="button secondary" id="refresh-models-button" type="button">${icons.refresh} ${t('获取最新模型列表', 'Refresh model list')}</button>
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

  const refreshButton = document.getElementById('refresh-models-button') as HTMLButtonElement
  refreshButton.addEventListener('click', () => void refreshModelList(refreshButton))

  onProbeModelChange(() => renderRows())
  void loadModelPage()
}
