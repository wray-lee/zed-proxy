// Models page: the catalogue, its refresh control and probe-model selection.

import { h, clear, pageRoot } from '../dom.ts'
import { describe, failureFrom } from '../api.ts'
import type { ModelRow } from '../data.ts'
import { providerName } from '../format.ts'
import { state } from '../state.ts'
import { tf, tr } from '../text.ts'
import { emptyBlock, errorBlock, loadingBlock, pageHead, toast } from '../ui.ts'
import {
  ensureCatalogue,
  isFailure,
  refreshCatalogueNow,
  reloadCatalogue,
  selectProbeModel,
  view,
} from '../store.ts'

const host = (): HTMLElement | null => pageRoot('models')

function levelChips(row: ModelRow): HTMLElement {
  const lang = state.lang
  return h(
    'div',
    { class: 'chip-row' },
    h('span', { class: 'chip-label' }, tr('defaultEffort', lang)),
    h('span', { class: 'chip' }, row.defaultEffort ?? '—'),
    h('span', { class: 'chip-label' }, tr('levels', lang)),
    ...(row.levels.length === 0
      ? [h('span', { class: 'chip' }, '—')]
      : row.levels.map((level) => h('span', { class: 'chip' }, level))),
    h('span', { class: 'chip-label' }, tr('provider', lang)),
    h('span', { class: 'chip' }, providerName(row.ownedBy) ?? tr('unknown', lang)),
  )
}

function modelRow(row: ModelRow): HTMLElement {
  const lang = state.lang
  const selected = row.slug === state.probeModel
  return h(
    'li',
    { class: 'model-row', 'data-selected': String(selected) },
    h(
      'div',
      { class: 'model-main' },
      h('p', { class: 'model-name' }, row.displayName),
      h('p', { class: 'model-slug' }, row.slug),
      row.description ? h('p', { class: 'model-desc' }, row.description) : null,
      levelChips(row),
      row.contextLength !== null
        ? h('p', { class: 'model-desc' }, `${row.contextLength} context tokens`)
        : null,
    ),
    selected
      ? h('span', { class: 'badge', 'data-tone': 'ok' }, tr('inUse', lang))
      : h(
          'button',
          {
            type: 'button',
            class: 'btn',
            'aria-label': `${tr('useForProbe', lang)}: ${row.slug}`,
            onclick: () => {
              selectProbeModel(row.slug)
              toast(tf('probeModelChanged', lang, row.slug), 'ok')
            },
          },
          tr('useForProbe', lang),
        ),
  )
}

function catalogueBody(): HTMLElement {
  const lang = state.lang
  const slice = view.catalogue
  if (slice.phase === 'error' && slice.models.length === 0) {
    return errorBlock({
      titleKey: null,
      title: tf('loadFailedTitle', lang, tr('loadFailedTitleModels', lang)),
      failure: slice.error ?? failureFrom(new Error('unknown')),
      onRetry: () => void reloadCatalogue(),
    })
  }
  if ((slice.phase === 'loading' || slice.phase === 'idle') && slice.models.length === 0) {
    return loadingBlock(tr('loadingDetail', lang))
  }
  if (slice.models.length === 0) {
    return emptyBlock(tr('catalogueEmpty', lang))
  }
  const missingSelection = !slice.models.some((row) => row.slug === state.probeModel)
  const list = h('ul', { class: 'model-list' })
  for (const row of slice.models) {
    try {
      list.append(modelRow(row))
    } catch (err) {
      console.error('[console] model row failed', row.slug, err)
    }
  }
  return h(
    'div',
    {},
    missingSelection ? h('p', { class: 'field-hint' }, tr('catalogueSelectedMissing', lang)) : null,
    slice.phase === 'error' && slice.models.length > 0
      ? errorBlock({
          titleKey: null,
          title: tf('loadFailedTitle', lang, tr('loadFailedTitleModels', lang)),
          failure: slice.error ?? failureFrom(new Error('unknown')),
          onRetry: () => void reloadCatalogue(),
        })
      : null,
    h('p', { class: 'section-note' }, tf('catalogueCount', lang, slice.models.length)),
    list,
  )
}

async function doRefresh(): Promise<void> {
  const lang = state.lang
  const result = await refreshCatalogueNow()
  if (isFailure(result)) {
    toast(`${tr('refreshFailed', lang)} ${describe(result, lang)}`, 'bad')
  } else if (result.refreshed) {
    toast(tf('refreshOk', lang, view.catalogue.models.length), 'ok')
  } else if (result.retryAfterS > 0) {
    toast(tf('refreshThrottled', lang, result.retryAfterS), 'warn')
  } else {
    toast(tf('refreshBuiltin', lang, result.count ?? view.catalogue.models.length), 'warn')
  }
  render()
}

function render(): void {
  const node = host()
  if (!node) return
  const lang = state.lang
  clear(node)
  node.append(
    pageHead('models-title', tr('modelsTitle', lang), tr('modelsLede', lang), [
      h(
        'button',
        {
          type: 'button',
          class: 'btn btn-primary',
          disabled: view.busy.catalogue,
          'aria-busy': String(view.busy.catalogue),
          onclick: () => void doRefresh(),
        },
        view.busy.catalogue ? tr('refreshCatalogueBusy', lang) : tr('refreshCatalogue', lang),
      ),
    ]),
    h('div', { class: 'banner' }, h('p', {}, tr('catalogueBanner', lang))),
    h(
      'section',
      { class: 'section' },
      h(
        'div',
        { class: 'section-head' },
        h('h2', { class: 'section-title' }, tr('catalogueHeading', lang)),
      ),
      catalogueBody(),
    ),
  )
}

let attached = false

export const attachModelsPage = {
  render,
  attach(): void {
    ensureCatalogue()
    if (!attached) {
      attached = true
      if (view.catalogue.phase === 'error') void reloadCatalogue()
    }
  },
}
