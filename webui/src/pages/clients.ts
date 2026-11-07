// Clients page: four copy-ready snippets built from runtime values.
// The model comes from the catalogue; the key is always a placeholder.

import { h, clear, pageRoot } from '../dom.ts'
import { state, pageOrigin } from '../state.ts'
import { tf, tr } from '../text.ts'
import type { TextKey } from '../text.ts'
import { pageHead, toast } from '../ui.ts'
import { ensureCatalogue, view } from '../store.ts'

const BASE_PLACEHOLDER = 'gateway-key'

function exampleModel(): string {
  const models = view.catalogue.models
  if (models.some((row) => row.slug === state.probeModel)) return state.probeModel
  const first = models[0]
  return first ? first.slug : '<model-id>'
}

function modelReady(): boolean {
  return view.catalogue.models.length > 0
}

interface SnippetSpec {
  title: TextKey
  note: TextKey
  body: () => string
}

function snippets(): SnippetSpec[] {
  const origin = pageOrigin()
  const model = exampleModel()
  return [
    {
      title: 'connectCodex',
      note: 'connectCodexNote',
      body: () =>
        [
          `[model_providers.zed-proxy]`,
          `name = "zed-proxy"`,
          `base_url = "${origin}/v1"`,
          `env_key = "ZED_PROXY_API_KEY"`,
          ``,
          `[profiles.zed-proxy]`,
          `model_provider = "zed-proxy"`,
          `model = "${model}"`,
          `model_reasoning_effort = "xhigh"`,
        ].join('\n'),
    },
    {
      title: 'connectClaude',
      note: 'connectClaudeNote',
      body: () =>
        [
          `ANTHROPIC_BASE_URL=${origin}`,
          `ANTHROPIC_AUTH_TOKEN=${BASE_PLACEHOLDER}`,
          `ANTHROPIC_MODEL=${model}`,
          `ANTHROPIC_SMALL_FAST_MODEL=${model}`,
          `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1`,
          `NO_PROXY=127.0.0.1,localhost`,
        ].join('\n'),
    },
    {
      title: 'connectOpencode',
      note: 'connectOpencodeNote',
      body: () =>
        JSON.stringify(
          {
            provider: {
              'zed-proxy': {
                npm: '@ai-sdk/openai-compatible',
                name: 'zed-proxy',
                options: { baseURL: `${origin}/v1`, apiKey: BASE_PLACEHOLDER },
                models: { [model]: { name: model } },
              },
            },
          },
          null,
          2,
        ),
    },
    {
      title: 'connectCurl',
      note: 'connectCurlNote',
      body: () =>
        [
          `curl -N ${origin}/v1/responses \\`,
          `  -H "Authorization: Bearer ${BASE_PLACEHOLDER}" \\`,
          `  -H "Content-Type: application/json" \\`,
          `  -d '{"model":"${model}","input":"hello","reasoning":{"effort":"xhigh"},"stream":true}'`,
        ].join('\n'),
    },
  ]
}

async function copySnippet(text: string): Promise<void> {
  const lang = state.lang
  try {
    if (!navigator.clipboard || !window.isSecureContext) throw new Error('clipboard unavailable')
    await navigator.clipboard.writeText(text)
    toast(tr('copied', lang), 'ok')
  } catch {
    toast(tr('copyFailed', lang), 'warn')
  }
}

/** Ctrl/Cmd+A inside a focused snippet selects the block, not the whole page. */
function selectBlock(pre: HTMLElement, event: KeyboardEvent): void {
  if (!(event.ctrlKey || event.metaKey) || event.key !== 'a' || event.altKey) return
  const selection = window.getSelection()
  if (!selection) return
  event.preventDefault()
  const range = document.createRange()
  range.selectNodeContents(pre)
  selection.removeAllRanges()
  selection.addRange(range)
}

function snippetBlock(spec: SnippetSpec): HTMLElement {
  const lang = state.lang
  const body = spec.body()
  return h(
    'section',
    { class: 'snippet' },
    h(
      'div',
      { class: 'snippet-head' },
      h('div', {}, h('p', { class: 'snippet-title' }, tr(spec.title, lang)), h('p', { class: 'snippet-note' }, tr(spec.note, lang))),
      h(
        'button',
        {
          type: 'button',
          class: 'btn btn-small',
          onclick: () => void copySnippet(body),
        },
        tr('copy', lang),
      ),
    ),
    h('pre', { tabindex: '0', onkeydown: (event: Event) => {
      if (event instanceof KeyboardEvent) selectBlock(event.currentTarget as HTMLElement, event)
    } }, h('code', {}, body)),
  )
}

function render(): void {
  const node = pageRoot('integration')
  if (!node) return
  const lang = state.lang
  clear(node)
  const ready = modelReady()
  node.append(
    pageHead('integration-title', tr('connectTitle', lang), tr('connectLede', lang)),
    h(
      'div',
      { class: 'banner' },
      h('p', { class: 'banner-title' }, tr('connectBase', lang)),
      h('p', {}, pageOrigin()),
      h('p', {}, tr('connectBaseNote', lang)),
      h('p', {}, ready ? tf('connectModelNote', lang, exampleModel(), '/v1/models') : tr('connectModelFallback', lang)),
      h('p', {}, tr('connectKeyNote', lang)),
    ),
  )
  for (const spec of snippets()) {
    try {
      node.append(snippetBlock(spec))
    } catch (err) {
      console.error('[console] snippet failed', spec.title, err)
    }
  }
  node.append(h('p', { class: 'section-note' }, tr('connectRestart', lang)))
}

export const attachClientsPage = {
  render,
  attach(): void {
    ensureCatalogue()
  },
}
