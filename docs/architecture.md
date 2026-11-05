[← Back to README](../README.md)

# Architecture

This page maps the modules and the two flows that matter: a model request and a login. It does not repeat the endpoint and configuration tables; see [API endpoints](api.md) and [Configuration](configuration.md) for those.

## Modules

| File | Role |
| --- | --- |
| `src/main.zig` | Entry point. Parses the subcommand (`serve`, `login`, `accounts`), applies `ZED_DATA_DIR`, reads `ZED_API_HOST` and `ZED_API_PORT`, and starts the server. |
| `src/server.zig` | HTTP server: one thread per connection, request parsing, authentication gate, route table, account and login handlers, model list, embedded web page. |
| `src/gateway_auth.zig` | Gateway key loading (`ZED_API_KEY`, `ZED_API_KEY_FILE`, `<data>/api_key`), constant-time key comparison, `Authorization` and `x-api-key` parsing, protected path prefixes, loopback detection, and the fail-closed bind policy. |
| `src/providers.zig` | Request conversion from OpenAI Chat, OpenAI Responses, and Anthropic Messages into Zed's `completions` payload, and response conversion back. Holds the fallback model-name table (a slug Zed lists is forwarded unchanged; only unknown names are rewritten), reasoning effort rules, GPT output budget floors, and the Gemini `thinkingLevel` and `thought` handling. |
| `src/proxy.zig` | Upstream transport for `cloud.zed.dev/completions`, Zed client header constants (`ZED_VERSION`, user agent, status-message headers, optional `ZED_SYSTEM_ID`), proxy discovery (`HTTPS_PROXY`, `ALL_PROXY`, Windows system proxy), and upstream failure parsing. |
| `src/zed.zig` | Token minting from `cloud.zed.dev/client/llm_tokens`, model catalog fetch, billing fetch from `client/users/me`, JWT expiry parsing, and the protocol-specific entry points that combine token, conversion, transport, and response conversion. |
| `src/stream.zig` | Streaming path. Spawns `curl`, parses the upstream event stream, and emits SSE in the client's protocol. Also owns the account failover loop for streams. |
| `src/accounts.zig` | Account pool: `accounts.json` loading and writing, active account persistence, cooldown state, try order, and failure classification. |
| `src/account_status.zig` | Credential-free summary of the `client/users/me` billing response. |
| `src/orb_quota.zig` | Orb portal link validation, token extraction, ledger response parsing and aggregation. Pure functions, no HTTP. |
| `src/auth.zig` | RSA keypair generation and OAEP-SHA256 decryption, using BCrypt on Windows and the `openssl` CLI elsewhere. Also the local-callback login flow. |
| `src/zed_login.zig` | Parsing of the pasted callback URL or query string, plus the `SlotState` helper for the single-login slot. Pure functions, no sockets or files. |
| `src/socket.zig` | Socket read and write wrappers for the Windows and POSIX paths, plus HTTP response writing. |
| `build.zig` | Builds the `zed-proxy` executable, embeds `webui/dist/index.html`, links Windows system libraries when targeting Windows, and registers every test module behind `zig build test`. |
| `webui/` | TypeScript management page, built by Vite into a single HTML file. |

## Request flow

1. `server.handleConnection` reads the request line and headers, and parses the body up to 16 MiB. Larger bodies get 413.
2. If any gateway key is configured and the path starts with `/v1/` or `/zed/`, the request must carry a matching key; otherwise it gets 401 before any route or upstream work.
3. `route` dispatches on method and path. The three proxy routes first validate the client's `reasoning_effort` for GPT-5.6 models, then branch on the `stream` flag.
4. Non-streaming: `handleProxy` builds a try order from the account pool, and for each account calls `zed.proxyChatCompletions`, `zed.proxyResponses`, or `zed.proxyMessages`. Each of those mints or reuses an LLM token, converts the body with `providers.buildZedPayload`, posts to `cloud.zed.dev/completions`, and converts the response back. On a failover-worthy error the account is benched and the next one is tried.
5. Streaming: `stream.handleStreamProxy` runs the same try order, and `doStreamProxy` runs `curl -siN` against the same endpoint, translating each upstream line into SSE for the client protocol. The `stream_ended` marker ends the response and stops the `curl` process.
6. Upstream failures are classified as auth, rate limit, or transient, which decides the cooldown. A `status.failed` line on an HTTP 200 is treated as a failed request, not a completion.

## Login flow

Two variants share the same keypair and decryption code.

1. A 2048-bit RSA keypair is generated. The public key is exported as PKCS#1 DER and base64url encoded.
2. The authorization URL `https://zed.dev/native_app_signin?native_app_port=<port>&native_app_public_key=<key>` is returned or opened.
3. After sign-in, the browser is redirected to the callback with `user_id` and an encrypted `access_token`.
4. Paste flow: the user pastes the full URL into the web page or `POST /zed/login/complete`. The server parses the query with `zed_login.parseCallbackPayload`, decodes the base64url ciphertext, and decrypts it with the in-memory private key.
5. Local callback flow: the server or the CLI listens on a loopback port and reads the same parameters directly from the request line.
6. `finalizeLogin` writes the credential to `accounts.json` through `accounts.addAccount`, reloads the pool, and makes the new account active.

Only one login may be in progress at a time. A paste slot and a legacy slot share a mutex so a concurrent request gets 409. The private key never leaves process memory, and the authorization is valid for 10 minutes.

## Data files

Everything lives in the data directory (the working directory after `ZED_DATA_DIR` is applied).

| File | Contents |
| --- | --- |
| `accounts.json` | Account map: name to `user_id`, the Zed credential JSON, and an optional `orb_url`. Written by login and by `POST /zed/accounts/orb`, read at startup. |
| `active_account.txt` | Name of the last active account, so a restart resumes on it instead of the first entry. |
| `api_key` | Optional gateway key file, used when neither `ZED_API_KEY` nor `ZED_API_KEY_FILE` is set. |
| `zed-proxy_req_*.json`, `zed-proxy_stream_*.json` | Short-lived request bodies handed to `curl` through `--data-binary @file`. Deleted after the request; the data directory must therefore be writable for proxying to work. |

Login also needs a writable temp directory for the ciphertext file used by `openssl` (`/tmp`, `/var/tmp`, or `/dev/shm`, first writable one). The private key itself is passed to `openssl` on stdin and is not written to disk.
