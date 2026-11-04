[← Back to README](../README.md)

# API endpoints

All routes are handled in `src/server.zig`. "Gateway key" means the request must pass the authentication described in [Authentication](authentication.md) when at least one key is configured.

| Method | Path | Gateway key | Purpose |
| --- | --- | --- | --- |
| POST | `/v1/responses` | yes | OpenAI Responses, streaming and non-streaming; no Gemini |
| POST | `/v1/chat/completions` | yes | OpenAI Chat Completions, streaming and non-streaming |
| POST | `/v1/messages` | yes | Anthropic Messages, streaming and non-streaming |
| any | `/v1/messages/count_tokens` | yes | Returns `{"input_tokens":0}` without counting |
| any | `/api/event_logging/batch` | no | Accepts client event batches and returns `{"status":"ok"}` |
| GET | `/v1/models` | yes | Model list in OpenAI format |
| GET | `/health` | no | Liveness probe, returns `{"status":"ok"}` |
| any | `/` | no | Web management page |
| GET | `/zed/accounts` | yes | Accounts plus scheduler state |
| GET | `/zed/accounts/status` | yes | Passive token and quota status, no model call |
| POST | `/zed/accounts/health` | yes | Low-cost model probe for one or all accounts |
| POST | `/zed/accounts/switch` | yes | Switch the active account |
| GET | `/zed/usage` | yes | Token claims for the active account |
| GET | `/zed/billing` | yes | Billing and plan data for the active account |
| POST | `/zed/login` | yes | Start a login; `{"mode":"paste"}` selects the paste flow |
| POST | `/zed/login/complete` | yes | Submit the pasted callback URL |
| GET | `/zed/login/status` | yes | Poll login state |
| OPTIONS | any path | no | CORS preflight, returns 200 with an empty body |

Other methods or paths return 404.
