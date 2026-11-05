[← 返回 README](../README.zh-CN.md)

# 客户端配置

以下都是示例配置，不是硬性要求。任何会发这三种请求格式之一的客户端都可以使用对应的链路。

## OmniRoute（OpenAI 兼容上游）

- Base URL：`http://<宿主机地址>:8001/v1`
- API key：与 `ZED_API_KEY` 相同的值
- Model：`gemini-4-argon`

Chat Completions 请求用 `reasoning_effort`（或 `reasoning.effort`）传思考档位：

| 客户端传入 | Gemini `thinkingLevel` |
| --- | --- |
| 不传 / `none` | 不设 level（`includeThoughts` 仍为 true） |
| `minimal` | `LOW`（`gemini-4-argon` 和 `gemini-3.8` 会拒绝 `MINIMAL`） |
| `low` | `LOW` |
| `medium` | `MEDIUM` |
| `high` / `xhigh` / `max` / 未知值 | `HIGH` |

思考内容以 `thought: true` 的 part 返回。非流式响应在 Chat Completions 上放到 `message.thinking` 与 `message.reasoning_content`，在 Anthropic Messages 上放到 `thinking` 块；流式响应在 Chat 上放到 `reasoning_content`，在 Anthropic 上放到 thinking 块。正文在两种情况下都与思考分开。在 OpenAI Responses 链路上，它作为 reasoning 输出项返回。

## Claude Code

```sh
export ANTHROPIC_BASE_URL=http://127.0.0.1:8001
export ANTHROPIC_AUTH_TOKEN=<与 ZED_API_KEY 相同的值>
export ANTHROPIC_MODEL=claude-sonnet-5
```

`ANTHROPIC_AUTH_TOKEN` 只有在配置了网关 key 时才会被校验；仅监听回环且没有 key 的实例可以填任意非空字符串。

## Codex

Base URL 用 `http://127.0.0.1:8001/v1`，API key 用网关 key，模型用 `GET /v1/models` 里的 slug。Codex 走 Responses 链路（`POST /v1/responses`），可到达 OpenAI、Anthropic 与 Google 模型。
