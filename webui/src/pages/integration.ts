import { icons } from '../icons'
import { showToast } from '../toast'
import { t } from '../i18n'

function copyText(text: string) {
  navigator.clipboard.writeText(text)
    .then(() => showToast(t('配置已复制到剪贴板', 'Configuration copied to clipboard')))
    .catch(() => showToast(t('复制失败，请手动选择代码', 'Copy failed. Select the code manually.')))
}

export function renderIntegration() {
  const host = `http://127.0.0.1:${location.port || '8001'}`
  const page = document.getElementById('page-integration')!

  page.innerHTML = `
    <div class="page-heading">
      <div>
        <span class="eyebrow">${t('复制即可使用', 'Copy and go')}</span>
        <h1>${t('客户端接入', 'Client setup')}</h1>
        <p>${t('重点适配 Codex 与 Claude Code，同时保留 OpenCode 和通用 OpenAI 客户端接入方式。', 'Optimized for Codex and Claude Code, with OpenCode and generic OpenAI clients supported too.')}</p>
      </div>
    </div>

    <div class="notice-banner neutral">
      <span>${icons.refresh}</span>
      <p>${t('修改客户端配置后，请完整退出并重新启动对应客户端，已有任务可能仍保留创建时的旧 Provider。', 'After changing a client config, fully quit and restart that client. Tasks already created may keep their original provider.')}</p>
    </div>

    <div class="integration-grid">
      <article class="integration-card featured">
        <header>
          <span class="integration-icon">${icons.code}</span>
          <div><span>${t('重点支持', 'Primary support')}</span><strong>Codex · Responses API</strong></div>
          <span class="recommended-badge">${t('推荐', 'Recommended')}</span>
        </header>
        <div class="integration-body">
          <p>${t('将以下内容合并到 ', 'Merge the following into ')}<code>%USERPROFILE%\.codex\config.toml</code>${t('。本地路由支持 none、low、medium、high 和 xhigh。', '. The local router supports none, low, medium, high and xhigh.')}</p>
          <pre class="code-panel" id="config-codex"><button class="copy-button" data-target="config-codex" type="button">${icons.copy} ${t('复制配置', 'Copy config')}</button>model_provider = "zed_local"
model = "gpt-5.6-sol"
model_reasoning_effort = "xhigh"

[model_providers.zed_local]
name = "Zed Local"
base_url = "${host}/v1"
wire_api = "responses"
requires_openai_auth = false</pre>
        </div>
      </article>

      <article class="integration-card featured violet">
        <header>
          <span class="integration-icon">${icons.activity}</span>
          <div><span>${t('重点支持', 'Primary support')}</span><strong>Claude Code · Messages API</strong></div>
          <span class="recommended-badge">${t('推荐', 'Recommended')}</span>
        </header>
        <div class="integration-body">
          <p>${t('将以下环境配置合并到 ', 'Merge the following environment settings into ')}<code>%USERPROFILE%\.claude\settings.json</code>${t('。示例使用 Sonnet 5 与 xhigh 档位。', '. The example uses Sonnet 5 with the xhigh effort level.')}</p>
          <pre class="code-panel" id="config-claude"><button class="copy-button" data-target="config-claude" type="button">${icons.copy} ${t('复制配置', 'Copy config')}</button>{
  "env": {
    "ANTHROPIC_BASE_URL": "${host}",
    "ANTHROPIC_AUTH_TOKEN": "dummy",
    "ANTHROPIC_MODEL": "claude-sonnet-5",
    "ANTHROPIC_DEFAULT_HAIKU_MODEL": "claude-sonnet-5",
    "CLAUDE_CODE_EFFORT_LEVEL": "xhigh",
    "NO_PROXY": "127.0.0.1,localhost",
    "no_proxy": "127.0.0.1,localhost"
  },
  "model": "claude-sonnet-5",
  "effortLevel": "xhigh"
}</pre>
        </div>
      </article>

      <article class="integration-card">
        <header>
          <span class="integration-icon">${icons.server}</span>
          <div><span>${t('兼容客户端', 'Compatible clients')}</span><strong>OpenCode · OpenAI Compatible</strong></div>
        </header>
        <div class="integration-body">
          <p>${t('完整模型与推理档位见 ', 'See ')}<code>GET /v1/models</code>${t('，也可以先复制以下最小配置。', ' for the full model and effort-level list, or copy the minimal config below.')}</p>
          <pre class="code-panel" id="config-opencode"><button class="copy-button" data-target="config-opencode" type="button">${icons.copy} ${t('复制配置', 'Copy config')}</button>{
  "$schema": "https://opencode.ai/config.json",
  "model": "zed-local/gpt-5.6-sol",
  "provider": {
    "zed-local": {
      "npm": "@ai-sdk/openai-compatible",
      "name": "Zed Local",
      "options": {
        "baseURL": "${host}/v1",
        "apiKey": "dummy"
      }
    }
  }
}</pre>
        </div>
      </article>

      <article class="integration-card">
        <header>
          <span class="integration-icon">${icons.send}</span>
          <div><span>${t('命令行验证', 'Command-line check')}</span><strong>${t('Responses 流式请求', 'Responses streaming request')}</strong></div>
        </header>
        <div class="integration-body">
          <p>${t('响应应依次出现 ', 'Expect ')}<code>response.created</code>${t('、文本增量和 ', ', text deltas and ')}<code>response.completed</code>${t('。', ' in that order.')}</p>
          <pre class="code-panel" id="config-curl"><button class="copy-button" data-target="config-curl" type="button">${icons.copy} ${t('复制命令', 'Copy command')}</button>curl -N ${host}/v1/responses \
  -H "Content-Type: application/json" \
  -d '{"model":"gpt-5.6-sol","input":"Reply with ok only","reasoning":{"effort":"xhigh"},"stream":true}'</pre>
        </div>
      </article>
    </div>
  `

  page.querySelectorAll<HTMLButtonElement>('.copy-button').forEach(button => {
    button.addEventListener('click', () => {
      const target = document.getElementById(button.dataset.target!)!
      const clone = target.cloneNode(true) as HTMLElement
      clone.querySelector('.copy-button')?.remove()
      copyText(clone.textContent?.trim() ?? '')
    })
  })
}
