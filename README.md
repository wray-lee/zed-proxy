English | [简体中文](README.zh-CN.md)

# ZedProxy

[![CI](https://github.com/wray-lee/zed-proxy/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/wray-lee/zed-proxy/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)

ZedProxy is an HTTP gateway that exposes the models Zed hosts (including `gemini-4-argon`) through the OpenAI Chat Completions, OpenAI Responses, and Anthropic Messages request formats.

Zed's model service expects its own request payloads and a signed-in Zed account. ZedProxy accepts the API shapes that coding clients already speak, converts each request to Zed's format, forwards it to `cloud.zed.dev` with an account credential from its pool, and converts the response back. Clients such as OmniRoute, Codex, and Claude Code can then use Zed-hosted models through their normal OpenAI or Anthropic configuration.

## Features

- OpenAI Chat Completions (`POST /v1/chat/completions`), streaming and non-streaming.
- OpenAI Responses (`POST /v1/responses`), streaming and non-streaming. This route does not carry Gemini models.
- Anthropic Messages (`POST /v1/messages`), streaming and non-streaming.
- Gemini thinking support: client `reasoning_effort` maps to Gemini `thinkingLevel`, and upstream parts marked `thought: true` are split into thinking output instead of visible text.
- Account pool with cooldowns and failover. Auth errors, rate limits, and repeated upstream errors bench an account for a period, and the next request is attempted on another account.
- Web management page served at `/`.
- Gateway API key authentication with a fail-closed bind rule: a non-loopback bind without any configured key refuses to start.
- Docker image and compose example.
- Paste-callback login for container and remote deployments.
- `/health` liveness endpoint.

## Quick start

```sh
cp docker-compose.example.yml docker-compose.yml
cp .env.example .env          # then set ZED_API_KEY to your own value
docker compose up -d --build
curl -s http://127.0.0.1:8001/health
```

The compose file builds from the current directory and tags the image `zed-proxy:local`. It publishes only `127.0.0.1:8001` on the host and stores data in `./data`. If the image already exists, remove the `build: .` line and keep `image: zed-proxy:local`.

On Linux, make the data directory writable by the container user (uid 10001):

```sh
chown -R 10001:10001 ./data
chmod 700 ./data
```

## Authentication

Gateway keys are read in this order, and the first source that produces a non-empty value wins:

1. `ZED_API_KEY` environment variable
2. `ZED_API_KEY_FILE` file
3. `<data dir>/api_key` (`/data/api_key` in the container)

Values are comma or newline separated; surrounding whitespace and empty items are dropped. Multiple keys can be configured at once. A file that cannot be read yields no keys; there is no fallback to the next source after a read failure.

Protected paths are those starting with `/v1/` or `/zed/`. A request to a protected path must carry either header:

```
Authorization: Bearer <key>
x-api-key: <key>
```

`/health` and `/` are public. So is `/api/event_logging/batch`, because it sits outside the two protected prefixes; it acknowledges client event batches and stores nothing. Browser `OPTIONS` preflight requests are also answered without a key.

If no key is configured at all and the service listens on loopback, authentication is disabled. This keeps local-only use as before. If the bind address is not loopback and no key is configured, the process prints a refusal message and exits with status 1 instead of exposing an unauthenticated proxy.

## Login

Login creates or updates `accounts.json` in the data directory. Zed's login flow sends the browser back to a callback port on the machine that started the login. In Docker that port is not reachable from the browser, so use the paste flow.

### Docker: paste callback

1. Open `http://127.0.0.1:8001/` and open the accounts page.
2. Click the button that adds a Zed account.
3. Click the authorization link and finish signing in on the Zed page.
4. The browser lands on `http://127.0.0.1:<ZED_LOGIN_PORT>/...`. This page not loading is expected inside the container.
5. Copy the complete URL from the address bar, paste it into the input on the accounts page, and submit. The account list refreshes with the new account.

`ZED_LOGIN_PORT` defaults to `58443`. It is only embedded in the authorization URL to form the callback address; nothing needs to listen on it in this mode. The authorization link is valid for 10 minutes, the RSA private key stays in process memory, one authorization maps to one login, and only one login can be in progress at a time.

### Host: command line

```sh
zig build
./zig-out/bin/zed-proxy login my-account
```

The command opens the authorization URL in the default browser, listens on a random loopback port for the callback, and saves the account as `my-account` when it arrives. The wait is bounded to 10 minutes.

### Local callback mode through the server

`POST /zed/login` without a `mode` field starts the same local callback flow inside the running server: it listens on a random `127.0.0.1` port, opens the browser, and waits up to 10 minutes. The web page always uses the paste flow, so this mode is useful only when the callback port is reachable, for example when the server runs directly on the host.

## Client setup

The following are example configurations, not requirements. Any client that speaks one of the three supported request formats can use the corresponding route.

### OmniRoute (OpenAI-compatible upstream)

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

Thinking content comes back as parts with `thought: true`. Non-streaming responses expose it as `message.thinking` and `message.reasoning_content` on Chat Completions and as a `thinking` block on Anthropic Messages. Streaming responses send it in `reasoning_content` (Chat) or a thinking block (Anthropic). Answer text stays separate in both cases.

### Claude Code

```sh
export ANTHROPIC_BASE_URL=http://127.0.0.1:8001
export ANTHROPIC_AUTH_TOKEN=<same value as ZED_API_KEY>
export ANTHROPIC_MODEL=claude-sonnet-5
```

`configs/claude.settings.json.example` shows the same settings in JSON form. The value of `ANTHROPIC_AUTH_TOKEN` is only checked when a gateway key is configured; on a loopback-only instance without keys it can be any non-empty string.

## Endpoints

All routes are handled in `src/server.zig`. "Gateway key" means the request must pass the authentication described above when at least one key is configured.

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

## Configuration

| Variable | Default | Meaning |
| --- | --- | --- |
| `ZED_API_HOST` | `127.0.0.1` | Bind address. `localhost` is accepted as an alias for `127.0.0.1`. A non-loopback address requires at least one gateway key. |
| `ZED_API_PORT` | `8001` | Bind port. A positional port (`zed-proxy serve 9000`) takes precedence. |
| `ZED_DATA_DIR` | unset (current directory) | Directory for `accounts.json`, `active_account.txt`, and `api_key`. It is created if missing and the process changes into it before loading state. |
| `ZED_API_KEY` | unset | Gateway API keys, comma or newline separated. |
| `ZED_API_KEY_FILE` | unset | File containing gateway API keys. |
| `ZED_LOGIN_PORT` | `58443` | Callback port embedded in the paste login authorization URL. |
| `ZED_SYSTEM_ID` | not set | `x-zed-system-id` sent when minting LLM tokens. Unset means the header is omitted. |
| `HTTPS_PROXY`, `ALL_PROXY` (also lowercase) | unset | Outbound HTTP proxy for upstream calls. When unset, the Windows system proxy is used on Windows. |

The image sets `ZED_API_HOST=0.0.0.0`, `ZED_API_PORT=8001`, `ZED_LOGIN_PORT=58443`, and `ZED_DATA_DIR=/data`. The compose file passes `ZED_API_KEY` from `.env`.

## Architecture

```
client -> gateway auth -> route -> providers (request conversion)
       -> account pool -> cloud.zed.dev/completions
       -> providers (response and stream conversion) -> client
```

- `src/server.zig` owns the HTTP server, the route table, authentication gating, account endpoints, login endpoints, and the model list.
- `src/gateway_auth.zig` loads keys, parses `Authorization` and `x-api-key`, and enforces the bind policy.
- `src/providers.zig` converts each client protocol to Zed's payload and converts the upstream response back. It also applies the Gemini thinking mapping.
- `src/accounts.zig` keeps the account pool, cooldowns, try order, and the persisted active account.
- `src/stream.zig` translates streaming upstream events into SSE for each client protocol.
- `src/proxy.zig` and `src/zed.zig` send requests to `cloud.zed.dev` with `curl` when a proxy is configured, or with Zig's HTTP client otherwise.
- `src/auth.zig` and `src/zed_login.zig` implement the RSA keypair and both login flows.
- `webui/` is a TypeScript page built into a single HTML file and embedded in the binary at compile time.
- `curl` is required by the streaming path and by every upstream call when a proxy is configured. `openssl` is required by login and decryption on Linux. The Docker image installs both.

See [docs/architecture.md](docs/architecture.md) for the module map, request flow, login flow, and data files.

## Known limitations

- The paste callback decrypts only with RSA-OAEP-SHA256. OmniRoute's Zed helper also falls back to RSA PKCS#1 v1.5. Which padding the real Zed callback uses is not verified with a real account.
- The web page offers only the paste flow. The local callback listener still exists and can be triggered with `POST /zed/login` or the CLI.
- Real Zed login callbacks and real `gemini-4-argon` calls are not verified with a real account. Every test and probe so far used constructed credentials or local fakes.
- The arm64 image was built and started only under QEMU. It has not run on real arm64 hardware.
- The Windows BCrypt login branch compiles but has not been run.
- `/v1/responses` does not carry Gemini models; only Chat Completions and Anthropic Messages route to Google.
- The proxy is single-user. There is no per-client quota, rate limit, or tenant isolation.
- Upstream requests imitate the Zed client, and the upstream protocol can change at any time. Long-term availability is not guaranteed.
- `ZED_VERSION` is fixed at compile time (`1.8.2`). A different upstream client version may change which models or plans are accepted.
- No system id is sent when minting tokens unless `ZED_SYSTEM_ID` is set. A plain account minted tokens and completed requests without it in the author's checks; if Zed answers a trial account with `trial_blocked`, set `ZED_SYSTEM_ID` to the id your Zed client registered under.
- Upstream credentials are passed to `curl` as `-H` arguments, so they appear in the process argument list and can be read by other users on the same host. See [SECURITY.md](SECURITY.md).

## Unofficial project and terms risk

ZedProxy is unofficial. It is not affiliated with, endorsed by, or supported by Zed Industries, Inc. "Zed" is used only to describe the upstream service this proxy talks to.

With an account configured, the proxy sends requests to Zed-operated endpoints on your behalf:

- `cloud.zed.dev/completions` carries `authorization`, `content-type`, `x-zed-version: 1.8.2`, `x-zed-client-supports-status-messages: true`, `x-zed-client-supports-stream-ended-request-completion-status: true`, and `user-agent: Zed/1.8.2 (<os>; <arch>)`.
- `cloud.zed.dev/client/llm_tokens` carries `authorization`, `content-type`, `x-zed-system-id` (only when `ZED_SYSTEM_ID` is set), and the same `user-agent`.
- `cloud.zed.dev/models` carries `authorization`, `x-zed-version`, and the same `user-agent`.
- `cloud.zed.dev/client/users/me` carries `authorization`, `accept`, `content-type`, and the same `user-agent`.
- Login opens `zed.dev/native_app_signin` with a generated public key and callback port. In local callback mode the browser is then redirected to `zed.dev/native_app_signin_succeeded`; in paste mode the callback URL is pasted back into the proxy instead.

These headers and identifiers exist to make the traffic look like the official client. The account's plan, trial state, and any abuse checks are evaluated against them.

Zed's Terms of Service (`legal/terms.md`, last updated March 2, 2026, published at https://zed.dev/terms) contain clauses relevant to this kind of use. The sections below are cited so you can read them in full; this is a list of citations, not a legal interpretation.

- Section 2.2 (Access Grant): the Service may be used only for internal business purposes or personal non-commercial purposes, in accordance with Zed's documentation, and the customer agrees to access the Service only through the mechanisms designated by Zed.
- Section 2.4(a): the customer will not provide access to, distribute, sell, or sublicense the Service to a third party.
- Section 2.4(b): the customer will not seek to access non-public APIs associated with the Service.
- Section 2.4(d): the customer will not interfere with the operation of the Service, circumvent access restrictions, or conduct security or vulnerability tests of the Service.
- Section 6 (Suspension of Service): Zed may immediately suspend access, including for a breach of Sections 2.2 to 2.4 or Section 5.
- Section 8 (Disclaimers; No Warranties by Zed): the software, service, and output are provided "as is" and "as available", without warranties.
- Section 9.2 (Termination): if the customer violates any provision, the customer is not authorized to access the Service and the Terms terminate automatically; Zed may also suspend or terminate access at any time for any reason, with or without notice.
- Section 12 (Indemnity): the customer will defend and indemnify Zed against claims arising from, among other things, unauthorized use or misuse of the Service, the customer data, use of output, or violation of the Terms.

Section 2.3 also incorporates the acceptable use policies listed at https://zed.dev/acceptable-use-policies, which include the Anthropic Usage Policy, the Google Generative AI Prohibited Use Policy, the OpenAI Usage Policies, and the GitHub Acceptable Use Policies (`legal/third-party-terms.md` in the Zed repository lists the same set).

In practical terms, this project can route your own account's traffic in a way Zed did not intend. Your account may be suspended or terminated, and you use the software at your own risk. Use only your own account, do not share access to your gateway, and do not expose it publicly. This section is not legal advice.

## Acknowledgements

- [csmsqj/zed-api](https://github.com/csmsqj/zed-api) is the base this project was derived from. It is MIT licensed, and its copyright notice is retained in [LICENSE](LICENSE).
- [yukmakoto/zed2api](https://github.com/yukmakoto/zed2api) PR #2 was read for the idea of mapping Gemini thinking parameters. It is a reference only; no code was copied, and that repository declares no license.
- TraeWorkAssistant was read for management UI ideas only; no code was copied.

## License

MIT, see [LICENSE](LICENSE). The original copyright belongs to csmsqj; modifications are copyright 2026 wray-lee.

## Development

See [CONTRIBUTING.md](CONTRIBUTING.md) for contribution rules.

```sh
zig build          # build zig-out/bin/zed-proxy
zig build test     # run the unit tests
docker build -t zed-proxy:local .
```

Zig 0.15.2 or newer is required. The web page must be built once with `npm ci && npm run build` inside `webui/` before `zig build`, because the binary embeds `webui/dist/index.html`.
