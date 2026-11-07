[← Back to README](../README.md)

# Architecture

This page maps the modules and the three flows that matter: a model request, a login, and an Orb balance refresh. It does not repeat the endpoint and configuration tables; see [API endpoints](api.md) and [Configuration](configuration.md) for those.

## Layering

Requests flow in one direction and responses come back the other way:

```
HTTP entry (router, auth gate, body limits)
      ↓
client protocol decoders (Chat / Responses / Anthropic) → NeutralRequest
      ↓
provider encoders (open_ai / anthropic / google) → provider request
      ↓
upstream client (reqwest, TLS in-process, proxy, timeouts, size caps)
      ↓
provider decoders → NeutralResponse / StreamEvent
      ↓
client protocol encoders → client JSON / SSE
```

The neutral layer is the only thing the two halves share. A client protocol decoder never knows which provider will run, and a provider never knows which client format asked. Adding a fourth client format or a fourth provider adds one module and touches no existing encoder: the three providers do not import each other, and neither do the three client codecs.

Provider dispatch reads the `provider` field of the model from Zed's catalogue. Only a name the catalogue does not list falls back to the `claude*` → Anthropic, `gpt-*` → OpenAI, `gemini*` → Google prefix rule, and that rule exists in exactly one place. There is no hardcoded model whitelist.

## Modules

| File | Role |
| --- | --- |
| `src/main.rs` | Entry point. Parses the subcommand (`serve`, `login`, `accounts`, `healthcheck`), prints usage, and hands off. |
| `src/config.rs` | Environment parsing and validation: bind host and port, login port, data directory, upstream base URL, outbound proxy. Pure helpers so the rules are testable without mutating the process environment. |
| `src/http/server.rs` | Listener and connection loop. Reads one request head plus its `Content-Length` body, dispatches through the router, and writes the response with an exact `Content-Length` for buffered bodies and close delimiting for streams. `axum::serve` is not used because hyper always chunks unknown-length bodies and always adds `Date`. |
| `src/http/router.rs` | Route table and handlers, including the entry guard (authentication → 16 MiB limit → `OPTIONS` handling → common CORS/Connection headers). |
| `src/http/auth.rs` | Gateway key loading (`ZED_API_KEY`, `ZED_API_KEY_FILE`, `<data>/api_key`), SHA-256 constant-time comparison, `Authorization`/`x-api-key` parsing, protected prefixes, loopback detection, fail-closed bind decision and the refusal message. |
| `src/http/body.rs` | Body cap, JSON object gate, the exact 413/404 error bodies and response constructors. |
| `src/http/static.rs` | The embedded gateway panel (`webui/dist/index.html` via `include_bytes!`), served at `GET /` with `Cache-Control: no-store`. Tests check that the bundle is a complete HTML document and contains each of the five page anchors. |
| `src/client/chat.rs`, `responses.rs`, `anthropic.rs` | One decoder and one encoder per client protocol: request JSON → `NeutralRequest`, and `NeutralResponse` / `StreamEvent` → client JSON or SSE. JSON is written by hand because key order is normative. |
| `src/neutral/` | The shared data types: request, message/content blocks, tools and tool choice, response, stream events, stop reasons and usage. |
| `src/provider/openai.rs`, `anthropic.rs`, `google.rs` | Per-provider encoding of the neutral request and decoding of upstream events. Each owns its effort rules, output budget and sampling rules, tool dialect and event shapes. `openai.rs` targets the Responses API, `anthropic.rs` the Messages API, `google.rs` `generateContent`. |
| `src/provider/google_schema.rs` | Cleans a client JSON Schema into the Gemini subset (allowed keys, `nullable` handling, unsupported constructs) and is the only caller of that dialect. |
| `src/provider/mod.rs` | Factory (`encoder_for`) and the tiny shared envelope helpers. |
| `src/upstream/client.rs` | The only module that talks to the network: `reqwest` with rustls, four endpoints, timeouts (120 s non-stream, 300 s stream, 15 s meta), size caps (4 MiB / 1 MiB), proxy from `HTTPS_PROXY`/`ALL_PROXY`, `Zed/1.8.2` user agent, and Zed client header constants. Also classifies `status.failed` wrappers and HTTP statuses. |
| `src/upstream/token.rs` | Per-account LLM token cache and exchange against `client/llm_tokens`, with a 60-second refresh margin; `organization_id` selection from `client/users/me`; `ZED_SYSTEM_ID` parsing (sent only when non-empty). |
| `src/upstream/models.rs`, `jwt.rs` | `/models` pull and catalogue rebuild; JWT payload/`exp` inspection (signature never verified) used for cache expiry and for the verbatim `/zed/usage` body. |
| `src/catalog.rs` | The slug → capability registry. Built from Zed's `/models` response or the embedded `src/models.json`; produces the `/v1/models` payload (OpenAI `data[]` plus Codex `models[]`), knows the `gpt-5.6` alias, the one model-name normalization table and the one-hour list cache with its 5-second refresh window. |
| `src/accounts/store.rs` | Account pool: tolerant `accounts.json` loading, atomic 0600 writes (temp file + rename), active account persistence, cooldown/attempt state, try order and success/failure bookkeeping. |
| `src/accounts/status.rs` | Scheduler rows for `/zed/accounts`, the passive token/billing check for `/zed/accounts/status`, and both fixed-shape response bodies. |
| `src/proxy/mod.rs` | Request orchestration: decode, validate effort, pick the provider, iterate accounts with cooldowns, one 401 token re-mint, error classification, non-streaming attempt loop and the streaming pre-read/commit shell. |
| `src/proxy/stream.rs` | SSE framing state, line buffering with the 64 KiB cap, empty-stream retry constants and the client-facing stream type. |
| `src/probe.rs` | The live-check request: fixed body, catalogue-driven effort and budget, one attempt without failover, success test, and write-back of the probe result to the account. |
| `src/orb.rs` | Orb link validation, token extraction and re-encoding, portal fetch with browser headers, ledger parsing and aggregation, the fixed credit JSON, the 300 s/60 s cache and the forced-refresh rules. |
| `src/login/mod.rs` | RSA 2048 keypair in-process, PKCS#1 DER public key, authorization URL, callback parsing, OAEP-SHA256 decryption, the single login slot with generations, the paste and local-callback flows and the CLI login. |
| `src/error.rs` | Error kinds (`request`, `auth`, `plan`, `rate_limit`, `transient`, `client_disconnect`), HTTP status and protocol error bodies. |
| `webui/` | Panel sources. `index.html` is the static shell (rail, top bar, five page containers, dialog, no-script fallback); `src/main.ts` does boot order, navigation and per-page render isolation; `src/text.ts` holds every visible string in Chinese and English; `src/state.ts`, `api.ts`, `data.ts`, `store.ts`, `format.ts` hold language and storage, requests and the gateway key, tolerant JSON parsing and the catalogue cache, shared view state, and formatting; `ui.ts`, `dom.ts`, `dialog.ts` hold the shared blocks; `src/pages/*.ts` holds one renderer per page; `src/selfcheck.ts` is a dev-only assertion script that is not part of the bundle; `src/theme.css` holds every style and the design tokens. Vite plus `vite-plugin-singlefile` inlines the script and CSS, so the build product is one `dist/index.html` with no external origins and fonts from system stacks. |
| `build.rs` | Runs before the crate compiles: it only points `cargo:rerun-if-changed` at `webui/dist/index.html` and fails the build with an explicit "build the WebUI first" message when that file is missing, so a binary can never ship a blank page. The bundle is not more than that: no asset directory, no runtime file reads. |
| `tools/genmodels.py`, `tools/catalog-fixture.json` | Regenerates the embedded fallback catalogue: the script reads the sanitised Zed `/models` fixture and writes `src/models.json` (key order matters; the three C3 limit fields are copied only when they are positive integers). Run `python3 -I tools/genmodels.py`; it is stdlib-only and resolves both paths relative to its own location. |

## Request flow

1. `serve_connection` reads the request line and headers, and up to 16 MiB of body (larger `Content-Length` is rejected from the header alone). Chunked bodies are not supported.
2. The entry guard authenticates `/v1/` and `/zed/` requests when at least one key is configured, then applies the 16 MiB limit, answers `OPTIONS`, and adds the common headers.
3. The router dispatches on method and path. A proxy route first parses the body as a JSON object, then branches on `stream`. For GPT-5.6 models on the two OpenAI routes, `reasoning_effort` is validated before any upstream call; `/v1/messages` is exempt.
4. `proxy::prepare` decodes the client body into a neutral request, normalizes the model through the catalogue, resolves the provider from the catalogue and picks the encoder.
5. Non-streaming: `handle_non_stream` builds the attempt order from the account pool and, for each account, mints or reuses an LLM token, encodes the provider request, posts to `completions` and decodes the response. On a failover-worthy error the account is charged and the next one is tried; request-level 400-class failures end the request immediately.
6. Streaming: `handle_stream` runs the same attempt order but pre-reads the upstream until the first model event, so every failure before the client sees a byte can still be retried (empty 200 streams, up to 2 times) or failed over. After the commit point the response headers are already sent, so later failures become error frames inside the stream.
7. A `status.failed` object on an HTTP 200 is a failed request, not a completion. A `stream_ended` line closes the upstream stream normally.
8. Failures are classified as request, auth, plan, rate limit or transient, which decides the cooldown, the retry and the failover (see [Limitations](limitations.md) for what is not verified against the real upstream).

## Login flow

Both variants generate a fresh keypair and use the same parsing and decryption code.

1. An RSA 2048-bit keypair (public exponent 65537) is generated in process. The public key is exported as PKCS#1 `RSAPublicKey` DER and base64url encoded without padding.
2. The authorization URL `https://zed.dev/native_app_signin?native_app_port=<port>&native_app_public_key=<key>` is returned (paste flow) or returned and printed (local flow). Nothing opens a browser; the URL is always in the response or log.
3. After sign-in, the browser lands on `http://127.0.0.1:<port>/?user_id=…&access_token=…`.
4. Paste flow: the user pastes the full URL into the page or submits it to `POST /zed/login/complete`. The server drops the fragment, parses the query (`+` and `%XX` decoded, last duplicate wins, `user_id` checked before `access_token`) and decrypts the base64url ciphertext with the in-memory private key using RSA-OAEP-SHA256.
5. Local callback flow: the server or the CLI listens on a loopback port, reads the same parameters from the request line, and answers `302` to `https://zed.dev/native_app_signin_succeeded`.
6. The credential is written to `accounts.json` through `AccountStore::add_account` (atomic temp file + rename), the pool is reloaded, and the new account becomes active.

Only one login may be in progress at a time; a second start gets 409. The private key never leaves process memory, never reaches a file or a child process, and the authorization is valid for 10 minutes. No `openssl` CLI and no temporary ciphertext file exist.

## Orb refresh flow

`POST /zed/accounts/orb/refresh` picks the target accounts (one named account or every account with a link), and for each one:

1. Checks the 5-second floor since the last real portal request for that account; inside the window it answers from cache with `retry_after_s`.
2. Fetches `api/v1/customer_from_link`, then `api/v1/customers/<id>/ledger_summary`, both with the browser-like headers, a 10-second timeout and a 1 MiB cap.
3. Aggregates the credit blocks into the fixed credit object and stores it with a 300-second TTL (60 seconds on failure). A failure keeps the last good numbers for `/zed/accounts/status` while the refresh response reports `credit.ok=false`.

All requests go through the in-process client with proxy settings from the environment; the token and the link never appear in a response, a log or a process argument list.

## Web UI embedding

The gateway panel is not a separate service and has no assets on disk at run time:

1. `cd webui && npm ci && npx tsc --noEmit && npm run build` type-checks the sources and writes one `webui/dist/index.html` with the JavaScript and CSS inlined. There is no `dist/assets/`, and the page references no external origin; fonts come from system stacks.
2. `build.rs` runs before the crate compiles. It registers `webui/dist/index.html` as a rebuild input and panics with an explicit message when the file is missing, so a binary cannot be produced without a page.
3. `src/http/static.rs` embeds that file with `include_bytes!` at compile time and serves it at `GET /` as `text/html; charset=utf-8` with `Cache-Control: no-store`. Its tests assert the bytes are a complete HTML document and contain each of the five page containers (`page-accounts`, `page-models`, `page-health`, `page-endpoints`, `page-integration`), so a half-built bundle fails `cargo test`.
4. Because the bytes are fixed at compile time, changing the panel means rebuilding the bundle and then the binary; the running process never reads `webui/` and needs no write access to it. The only data the page keeps client-side are the three storage keys `zed_lang`, `zed_api_key` and `zed_probe_model`. The bundle is a single self-contained document: the five page containers named above, one inline script and one inline stylesheet, no linked assets and no external origin.
5. The same properties are what the acceptance run measures: the five page containers appear once each, a `file://` copy renders offline, and `build.rs` refuses a build when the bundle is absent. Acceptance logs and screenshots live under `F:\Projects\tmp\.scratch\zed-proxy\webui-check\rust-0.3.0-redesign\` (evidence only, not committed); the 0.3.0 evidence was re-measured on the release commit with bundle sha256 `67c30077362d4bd3058b8fa89ef4ff0a8fcf877b40a018abd67b127e4ff8a1aa` (the acceptance matrix records the exact commit).

## Data files

Everything lives in the data directory (the working directory after `ZED_DATA_DIR` is applied).

| File | Contents |
| --- | --- |
| `accounts.json` | Account map: name to `user_id`, the Zed credential JSON, and an optional `orb_url`. Written by login and by `POST /zed/accounts/orb`, read at startup; written atomically through a 0600 temp file and rename. |
| `active_account.txt` | Name of the last active account, so a restart resumes on it instead of the first entry. |
| `api_key` | Optional gateway key file, used when neither `ZED_API_KEY` nor `ZED_API_KEY_FILE` is set. |
| `.write_probe` | Created `0600` and immediately deleted before a login, to prove the directory is writable. |
| `.accounts.json.tmp`, `.active_account.txt.tmp` | Same-directory temp files used by the atomic writes; removed by the rename. |

The 0.2.2 `zed-proxy_req_*.json`, `zed-proxy_stream_*.json` and `zed-proxy_enc_<pid>_<n>.bin` files are gone: request bodies and ciphertext never touch the disk.
