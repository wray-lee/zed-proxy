[← Back to README](../README.md)

# Client setup

The following are example configurations, not requirements. Any client that speaks one of the three supported request formats can use the corresponding route.

## OmniRoute (OpenAI-compatible upstream)

- Base URL: `http://<host>:8001/v1`
- API key: the same value as `ZED_API_KEY`
- Model: `gemini-4-argon`

Chat Completions requests carry the thinking level in `reasoning_effort` (or `reasoning.effort`):

| Client value | Gemini `thinkingLevel` |
| --- | --- |
| not set / `none` | not set (`includeThoughts` stays true) |
| `minimal` | `LOW` (`gemini-4-argon` and `gemini-3.8` reject `MINIMAL`) |
| `low` | `LOW` |
| `medium` | `MEDIUM` |
| `high` / `xhigh` / `max` / unknown | `HIGH` |

Thinking content comes back as parts with `thought: true`. Non-streaming responses expose it as `message.thinking` and `message.reasoning_content` on Chat Completions and as a `thinking` block on Anthropic Messages. Streaming responses send it in `reasoning_content` (Chat) or a thinking block (Anthropic). Answer text stays separate in both cases. On the OpenAI responses route it comes back as a reasoning output item.

## Claude Code

```sh
export ANTHROPIC_BASE_URL=http://127.0.0.1:8001
export ANTHROPIC_AUTH_TOKEN=<same value as ZED_API_KEY>
export ANTHROPIC_MODEL=claude-sonnet-5
```

The value of `ANTHROPIC_AUTH_TOKEN` is only checked when a gateway key is configured; on a loopback-only instance without keys it can be any non-empty string.

## Codex

Use `http://127.0.0.1:8001/v1` as the base URL, the gateway key as the API key, and a model slug from `GET /v1/models`. Codex uses the Responses route (`POST /v1/responses`), which reaches OpenAI, Anthropic and Google models.
