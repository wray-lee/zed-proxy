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
| GET | `/v1/models` | yes | Zed's full model catalogue in OpenAI format, plus a Codex `models` catalogue. Cached for an hour; nothing is filtered except models Zed marks disabled. The bare `gpt-5.6` alias for Sol is added. |
| GET | `/health` | no | Liveness probe, returns `{"status":"ok"}` |
| any | `/` | no | Web management page |
| GET | `/zed/accounts` | yes | Accounts plus scheduler state |
| GET | `/zed/accounts/status` | yes | Passive token and quota status, plus the cached Orb credit balance, no model call |
| POST | `/zed/accounts/health` | yes | Low-cost model probe for one or all accounts, on a chosen model |
| POST | `/zed/accounts/orb` | yes | Set or clear an account's Orb portal link |
| POST | `/zed/accounts/switch` | yes | Switch the active account |
| GET | `/zed/usage` | yes | Token claims for the active account |
| GET | `/zed/billing` | yes | Billing and plan data for the active account |
| POST | `/zed/login` | yes | Start a login; `{"mode":"paste"}` selects the paste flow |
| POST | `/zed/login/complete` | yes | Submit the pasted callback URL |
| GET | `/zed/login/status` | yes | Poll login state |
| OPTIONS | any path | no | CORS preflight, returns 200 with an empty body |

Other methods or paths return 404.

## `POST /zed/accounts/health`

The body may be `{}` for all accounts or `{"account":"<name>"}` for one, either with an optional `"model":"<slug>"`. Without `account` every account is probed sequentially.

`model` must be a slug from `GET /v1/models`. An unknown slug returns `400 {"error":"unknown model"}`. The default is `gpt-5.6-luna`. The probe is a single low-cost request built for the chosen model: GPT-5.6 models send `reasoning_effort: "none"`, Claude models omit the field, Gemini models send `"low"` and are never sent `MINIMAL`, and GPT-5.5 omits the field because the conversion layer pins it to `xhigh`.

The response reports the model, the reasoning effort and the output budget actually used, so all three change with the selection. `max_output_tokens` is `null` for Gemini, whose request carries no output cap, and the GPT-5.5 family reports `32768` because the conversion layer pins its effort to `xhigh` and lifts the budget to the large-reasoning floor. With the default:

```json
{"checked_at":1762300000,"probe":{"model":"gpt-5.6-luna","reasoning_effort":"none","max_output_tokens":32},"accounts":[...]}
```

## `POST /zed/accounts/orb`

Stores or clears the Orb portal link used for the credit balance display:

```json
{"account":"my-account","url":"https://portal.withorb.com/?token=<orb-token>"}
```

The URL must be `https`, the host must be exactly `portal.withorb.com` with no userinfo and no port other than the default `443`, and it must carry a non-empty `token` query parameter; otherwise the route returns `400 {"error":"invalid orb link"}`. An empty `url` clears the stored link. An unknown account returns 404, success returns `200 {"success":true}`. The link is never echoed back in any response.

## `GET /zed/accounts/status`

Each account entry carries the fields below in addition to the token and quota fields:

| Field | Meaning |
| --- | --- |
| `orb_configured` | `true` when an Orb portal link is stored for the account |
| `credit` | Present only when `orb_configured` is true |

`credit` is `{"ok":true,"balance_usd":7.5,"cap_usd":10.0,"used_usd":2.5,"used_pct":25,"period_start":"2026-11-04T00:00:00+00:00","resets_at":"2026-12-04T00:00:00+00:00","total_balance_usd":7.5}` on success, and `{"ok":false,"error":"<short code>"}` when the fetch fails, where the code is one of `invalid_link`, `timeout`, `parse_error` or `fetch_failed`. `used_pct` is the used share of the cap, in percent.

Balance data is fetched only for accounts with a configured link, one at a time, with a 10 second timeout per fetch, and cached per account for 5 minutes; a failed fetch is cached for 60 seconds. A status poll therefore usually serves cached numbers, and the link or token never appears in a response.
