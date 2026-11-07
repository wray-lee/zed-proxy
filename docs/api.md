[← Back to README](../README.md)

# API endpoints

All routes are handled by the router in `src/http/router.rs`. "Gateway key" means the request must pass the authentication described in [Authentication](authentication.md) when at least one key is configured. Requests to `/v1/` and `/zed/` are checked before routing, so a protected path with a missing or wrong key gets 401 rather than 404.

| Method | Path | Gateway key | Purpose |
| --- | --- | --- | --- |
| any | `/` | no | Gateway panel (single embedded HTML page) |
| GET | `/health` | no | Liveness probe, returns `{"status":"ok"}` |
| GET | `/v1/models` | yes | Zed's full model catalogue in OpenAI format, plus a Codex `models` catalogue. Cached for an hour; nothing is filtered except models Zed marks disabled. The bare `gpt-5.6` alias for Sol is added. |
| any | `/api/event_logging/batch` | no | Accepts client event batches and returns `{"status":"ok"}` |
| any | `/v1/messages/count_tokens…` | yes | Prefix route (Claude Code appends suffixes); returns `{"input_tokens":0}` without counting |
| POST | `/v1/chat/completions` | yes | OpenAI Chat Completions, streaming and non-streaming |
| POST | `/v1/responses` | yes | OpenAI Responses, streaming and non-streaming |
| POST | `/v1/messages` | yes | Anthropic Messages, streaming and non-streaming |
| GET | `/zed/accounts` | yes | Accounts plus scheduler state |
| GET | `/zed/accounts/status` | yes | Passive token and quota status, plus the cached Orb credit balance, no model call |
| POST | `/zed/accounts/health` | yes | Low-cost model probe for one or all accounts, on a chosen model |
| POST | `/zed/accounts/orb` | yes | Set or clear an account's Orb portal link |
| POST | `/zed/accounts/orb/refresh` | yes | Force-refresh the Orb credit balance (new in 0.3.0) |
| POST | `/zed/accounts/switch` | yes | Switch the active account |
| POST | `/zed/models/refresh` | yes | Force-refresh the model catalogue (new in 0.3.0) |
| GET | `/zed/usage` | yes | The active account's LLM-token JWT payload (claims only, never the token) |
| GET | `/zed/billing` | yes | The active account's raw `client/users/me` body from Zed |
| POST | `/zed/login` | yes | Start a login; `{"mode":"paste"}` selects the paste flow |
| POST | `/zed/login/complete` | yes | Submit the pasted callback URL |
| GET | `/zed/login/status` | yes | Poll login state |
| OPTIONS | any path | no | CORS preflight; 200 with an empty body, except the three routes below |

`OPTIONS /`, `OPTIONS /api/event_logging/batch` and `OPTIONS /v1/messages/count_tokens…` return their normal non-empty bodies. Methods other than the ones listed (and other paths) return 404; there is no 405.

Every response carries `Access-Control-Allow-Origin: *`, `Access-Control-Allow-Headers: *`, `Access-Control-Allow-Methods: GET, POST, OPTIONS` and `Connection: close`. Responses are close-delimited, never chunked, and carry no `Date`, `Server` or `Transfer-Encoding` header. `GET /` adds `Cache-Control: no-store`; SSE responses add `Cache-Control: no-cache`.

Requests with a `Content-Length` above 16 MiB get `413 {"error":{"message":"request body exceeds 16 MiB","type":"invalid_request_error"}}` before the body is read. Chunked request bodies are not supported and are treated as empty.

## `GET /v1/models`

Two lists in one object:

- `data[]`: `{"id","object":"model","owned_by"}`, plus `context_length`, `max_output_tokens` and `max_completion_tokens` when the catalogue has a positive value for them. The fields are appended after `owned_by`; a missing, null or non-positive value omits the field instead of inventing a default. `max_completion_tokens` equals `max_output_tokens`. The `gpt-5.6` alias is first and copies every value from `gpt-5.6-sol`.
- `models[]`: the Codex shape (`slug`, `display_name`, `description` prefixed with `Zed - `, `default_reasoning_level` when the model declares one, `supported_reasoning_levels`, `shell_type`, `visibility`, `supported_in_api`, `priority`). It does not carry the three numeric fields and excludes disabled models.

Example:

```json
{"object":"list","data":[{"id":"gpt-5.6","object":"model","owned_by":"open_ai","context_length":400000,"max_output_tokens":128000,"max_completion_tokens":128000},{"id":"claude-sonnet-5-5","object":"model","owned_by":"anthropic","context_length":1000000,"max_output_tokens":128000,"max_completion_tokens":128000}],"models":[...]}
```

The route always answers 200. When the upstream pull fails it serves the previous cache, then the built-in catalogue; without any account it serves the built-in catalogue.

## Proxy routes: `POST /v1/chat/completions`, `/v1/responses`, `/v1/messages`

The body must be a JSON object; anything else is 400 with `invalid JSON request body` in the client protocol's shape (both "not JSON" and "valid JSON but not an object" produce the same bytes). `stream` is the JSON boolean `true` for streaming; any other value or a missing key is non-streaming.

Errors follow the client protocol:

- OpenAI routes (Chat and Responses): `{"error":{"message":"…","type":"…","param":null[,"code":"…"]}}`
- Anthropic route: `{"type":"error","error":{"type":"…","message":"…"}}`

Status codes:

| Situation | HTTP | Notes |
| --- | --- | --- |
| Body is not a JSON object | 400 | `invalid JSON request body` |
| Unsupported `reasoning_effort` for a GPT-5.6 model on an OpenAI route | 400 | `code:"unsupported_reasoning_effort"`; `/v1/messages` is exempt |
| No account configured | 400 | `no account configured` |
| Upstream request-level failure (`400`/`404`/`405`/`413`/`415`/`422`, including a `status.failed` wrapper on HTTP 200) | 400 | The upstream `message` is preserved; not retried, not cooled down, not failed over |
| Credential failure (401, non-plan 403) | 401 / 403 | Account cooled down; failover to the next account |
| Plan-level 403 | 403 | Not cooled down; failover to the next account |
| Rate limit (429) | 429 | Account cooled down for 30 s |
| Transient failure (5xx, network, empty stream) | 502 | Account cooled down after 3 consecutive failures |

Streaming responses are `text/event-stream` with `Cache-Control: no-cache` and `Connection: close`. An empty HTTP 200 upstream stream is retried up to twice per account before failing over. Once the first model event has been committed, later failures are delivered inside the stream (an error frame followed by the protocol's end marker) and never switch accounts.

## `POST /zed/accounts/health`

The body may be `{}` (or empty) for all accounts or `{"account":"<name>"}` for one, either with an optional `"model":"<slug>"`. Without `account` every account is probed sequentially.

`model` must be a slug from `GET /v1/models` or the `gpt-5.6` alias. An unknown slug returns `400 {"error":"unknown model"}`; a non-string returns `400 {"error":"model must be a string"}`. Invalid JSON returns `400 {"error":"invalid json"}` and a non-object body `400 {"error":"request body must be an object"}`. An unknown account returns `404 {"error":"account not found"}`. The default is `gpt-5.6-luna`.

The probe is a single low-cost request built for the chosen model, with effort derived from the catalogue:

| Model kind | Sent `reasoning_effort` | Reported | Reported output budget |
| --- | --- | --- | --- |
| Anthropic provider | omitted | `null` | `min(32, catalogue max)` |
| GPT-5.5 family | omitted (the encoder pins `xhigh`) | `"xhigh"` | `32768` |
| Catalogue declares `none` / `low` | that value | that value | `32` |
| OpenAI model with no effort levels (e.g. `gpt-5-nano`) | omitted | `null` | `1024` (catalogue max capped) |
| Google provider | `"low"` (never `MINIMAL`) | `"low"` | `min(max(32, 32), catalogue max)`, i.e. `32` |

The response reports the model, the reasoning effort and the output budget actually used. Key order is fixed:

```json
{"checked_at":1762300000,"probe":{"model":"gpt-5.6-luna","reasoning_effort":"low","max_output_tokens":32},"accounts":[{"name":"alice","model_ok":true,"model_state":"healthy","model_checked_at":1762300000,"model_latency_ms":640,"scheduler_healthy":true,"cooldown_s":0,"last_status":200}]}
```

`model_state` is one of `unchecked`, `healthy`, `auth_error`, `rate_limited`, `upstream_error`; `model_ok` equals `model_state == "healthy"`. Probe failures still answer 200 and are charged to the account's cooldown state like request failures.

## `POST /zed/accounts/orb`

Stores or clears the Orb portal link used for the credit balance display:

```json
{"account":"my-account","url":"https://portal.withorb.com/?token=<orb-token>"}
```

The URL must be `https`, the host must be exactly `portal.withorb.com` with no userinfo and no port other than the default `443`, and it must carry a non-empty `token` query parameter; otherwise the route returns `400 {"error":"invalid orb link"}`. An absent `url`, `null` or an empty string clears the stored link. An unknown account returns `404 {"error":"account not found"}`, success returns `200 {"success":true}`. Errors: `400 {"error":"invalid json"}`, `400 {"error":"request body must be an object"}`, `400 {"error":"missing account"}`, `400 {"error":"account must be a string"}`, `400 {"error":"url must be a string"}`. The link is never echoed back in any response.

## `POST /zed/accounts/orb/refresh`

Forces a portal fetch, bypassing the 5-minute success / 60-second failure cache, with a 5-second minimum interval per account:

- Request: empty body, `{}`, or `{"account":"<name>"}`.
- `400 {"error":"invalid json"}`, `400 {"error":"request body must be an object"}`, `400 {"error":"account must be a string"}`, `400 {"error":"orb link not configured"}`, `404 {"error":"account not found"}`.
- A refresh inside the 5-second window does not touch the portal and answers with the cached credit plus `retry_after_s` (0–5).

Single account:

```json
{"refreshed":true,"account":"alice","retry_after_s":0,"credit":{"ok":true,"balance_usd":7.5000,"cap_usd":10.0000,"used_usd":2.5000,"used_pct":25.00,"period_start":"2026-11-01T00:00:00+00:00","resets_at":"2026-12-01T00:00:00+00:00","total_balance_usd":7.5000}}
```

All accounts (account-file order; only accounts with a link):

```json
{"refreshed":true,"accounts":[{"account":"alice","refreshed":true,"retry_after_s":0,"credit":{...}},{"account":"bob","refreshed":false,"retry_after_s":3,"credit":{...}}]}
```

With no account configured, or no account carrying a link: `200 {"refreshed":false,"accounts":[]}`.

A portal failure still answers 200 with `credit.ok=false` and one of `invalid_link`, `timeout`, `parse_error`, `fetch_failed`; the previous successful numbers stay cached for `/zed/accounts/status`.

## `GET /zed/accounts/status`

Each account entry carries the token, billing, probe and scheduling fields plus:

| Field | Meaning |
| --- | --- |
| `orb_configured` | `true` when an Orb portal link is stored for the account |
| `credit` | Present only when `orb_configured` is true |

`credit` is `{"ok":true,"balance_usd":7.5,"cap_usd":10.0,"used_usd":2.5,"used_pct":25,"period_start":"2026-11-01T00:00:00+00:00","resets_at":"2026-12-01T00:00:00+00:00","total_balance_usd":7.5}` on success, and `{"ok":false,"error":"<short code>"}` when the fetch fails. `used_pct` is the used share of the cap, in percent.

`quota_state` is one of `available`, `exhausted`, `restricted`, `unmetered`, `unavailable` or `unknown`. When the token refresh fails the entry ends with `error:"llm_token_refresh_failed"` and no billing fields follow.

Balance data is fetched only for accounts with a configured link, one at a time, with a 10 second timeout per fetch, and cached per account for 5 minutes; a failed fetch is cached for 60 seconds. A status poll therefore usually serves cached numbers, and the link or token never appears in a response.

## `POST /zed/accounts/switch`

Request `{"account":"<name>"}`. Success: `200 {"success":true}`, and the choice is persisted to `active_account.txt`. Unknown name: `404 {"error":"not found"}`. Malformed bodies: `400 {"error":"invalid json"}`, `400 {"error":"request body must be an object"}`, `400 {"error":"missing account"}`, `400 {"error":"bad type"}`. Switching does not change cooldown state and does not check that the target is healthy.

## `POST /zed/models/refresh`

Forces a catalogue pull from Zed, with a 5-second minimum interval:

| Situation | HTTP | Body |
| --- | --- | --- |
| Success | 200 | `{"refreshed":true,"count":N}` |
| Repeated within 5 seconds | 200 | `{"refreshed":false,"retry_after_s":s,"count":N}` |
| No account | 200 | `{"refreshed":false,"count":N}` (built-in catalogue) |
| Upstream failure (old catalogue kept) | 502 | `{"error":"models refresh failed","refreshed":false}` |
| Body is not valid JSON | 400 | `{"error":"invalid json"}` |
| Body is JSON but not an object | 400 | `{"error":"request body must be an object"}` |

`count` is the number of slugs in the registry (the `gpt-5.6` alias and disabled models excluded). A successful refresh also rebuilds the `/v1/models` cache.

## `GET /zed/usage`

Returns the LLM token's JWT payload, with the signature and the full token left out. The payload is the base64url-decoded bytes exactly as the token carries them: no re-encoding, no key reordering, no whitespace changes (the body is still valid JSON because the token's payload was):

| Situation | HTTP | Body |
| --- | --- | --- |
| Account present, token minted or cached | 200 | the decoded payload bytes, e.g. `{"sub":"42","name":"alice","exp":1762300000}` |
| No active account | 400 | `{"error":"no account"}` |
| Token could not be minted or the token is malformed | 500 | `{"error":"internal error"}` |

## `GET /zed/billing`

Fetches `client/users/me` for the active account and returns the upstream body unchanged: no re-encoding, no key reordering, no field removal. The body can carry the account e-mail and plan, so it is protected by the gateway key.

| Situation | HTTP | Body |
| --- | --- | --- |
| Account present, upstream 2xx | 200 | the raw `client/users/me` body |
| No active account | 400 | `{"error":"no account"}` |
| Upstream non-2xx, transport failure, or a body above 1 MiB | 502 | `{"error":"failed to fetch user info"}` |

## Login routes

`POST /zed/login` accepts an optional JSON object. `{"mode":"paste","name":"work"}` starts the paste flow and returns `{"login_url":"https://zed.dev/native_app_signin?native_app_port=58443&native_app_public_key=<b64url>","port":58443,"mode":"paste","expires_in":600}`. Any other body (including an invalid one) starts the local-callback flow and returns `{"login_url":"…","port":<real port>}`. A second login while one is in progress returns `409 {"error":"login already in progress"}`; an invalid `ZED_LOGIN_PORT` returns `400 {"error":"ZED_LOGIN_PORT must be 1-65535"}`; an unwritable data directory returns 500 with `{"error":"data directory is not writable","detail":"accounts.json cannot be saved; on Linux run chown -R 10001:10001 ./data or set ZED_DATA_DIR"}`.

`POST /zed/login/complete` takes `{"url":"<callback URL>"}` (or `{"query":"…"}`) and returns `{"status":"success","name":"<account>"}`. `400 {"error":"invalid json"}`, `400 {"error":"missing url"}`, the parse and decrypt errors listed in [Login](login.md), `409 {"error":"no pending login"}`, `410 {"error":"pending login expired; start again"}`, and save failures (`400` for a too-long name or a non-JSON credential, `500 {"error":"failed to save account: <short code>"}`).

`GET /zed/login/status` returns `{"status":"idle"}`, `{"status":"waiting"}`, `{"status":"success"}` or `{"status":"failed"}`; `success` and `failed` are consumed on read. The route never returns account names, URLs or keys.
