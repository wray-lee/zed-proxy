<div align="center">

<img src="docs/assets/logo.svg" alt="ZedProxy logo" width="96" height="96">

# ZedProxy

**Use Zed-hosted models, including Gemini 4, from any OpenAI, Anthropic or Codex client.**

<a href="README.zh-CN.md">简体中文</a> | English

[![CI](https://github.com/wray-lee/zed-proxy/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/wray-lee/zed-proxy/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/wray-lee/zed-proxy?sort=semver)](https://github.com/wray-lee/zed-proxy/releases)
[![Docker image](https://img.shields.io/badge/ghcr.io-wray--lee%2Fzed--proxy-2496ED?logo=docker&logoColor=white)](https://github.com/wray-lee/zed-proxy/pkgs/container/zed-proxy)
[![Rust](https://img.shields.io/badge/Rust-1.99-000000?logo=rust&logoColor=white)](https://www.rust-lang.org)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)

<a href="#quick-start">Quick start</a> · <a href="#documentation">Documentation</a> · <a href="docs/architecture.md">Architecture</a> · <a href="CONTRIBUTING.md">Contributing</a>

<img src="docs/assets/webui-accounts.en.png" alt="ZedProxy gateway panel, accounts page" width="860">

<sub>The accounts page: scheduling order, token and subscription plan state, Orb balance and live model checks per account. Screenshot from the fake-data test gateway; no real account or credential appears.</sub>

</div>

ZedProxy is an HTTP gateway that exposes the models Zed hosts (including `gemini-4-argon`) through the OpenAI Chat Completions, OpenAI Responses, and Anthropic Messages request formats.

Zed's model service expects its own request payloads and a signed-in Zed account. ZedProxy accepts the API shapes that coding clients already speak, converts each request to Zed's format with a provider-specific encoder, forwards it to `cloud.zed.dev` with an account credential from its pool, and converts the response back. Clients such as OmniRoute, Codex, and Claude Code can then use Zed-hosted models through their normal OpenAI or Anthropic configuration.

This is the 0.3.0 Rust implementation. It runs as a single static binary: HTTP, TLS, streaming and RSA login all happen in-process, with no `curl` or `openssl` subprocess. Only Linux and containers are supported.

> Unofficial project, not affiliated with or endorsed by Zed Industries. See [Terms and risk](docs/terms-risk.md).

## Features

- OpenAI Chat Completions (`POST /v1/chat/completions`), streaming and non-streaming.
- OpenAI Responses (`POST /v1/responses`), streaming and non-streaming.
- Anthropic Messages (`POST /v1/messages`), streaming and non-streaming.
- All three routes can reach all three upstream providers (OpenAI, Anthropic and Google models). Providers are separate modules behind one neutral request/response type, and dispatch follows the provider named in Zed's model catalogue.
- Gemini support: client `reasoning_effort` maps to Gemini `thinkingLevel`, upstream parts marked `thought: true` are split into thinking output instead of visible text, tool schemas are cleaned to the Gemini subset, and tool calls round-trip in both directions.
- Account pool with cooldowns and failover. Auth errors, rate limits, and repeated transient upstream errors bench an account for a period, and the next request is attempted on another account.
- Request-level failures (upstream `400`/`404`/`413`/`415`/`422`, including `status.failed` wrappers) are returned as `400` with the upstream message and are never retried, cooled down, or failed over.
- Gateway panel served at `/`, in English and Chinese, with five pages: account pool, model list, diagnostics, routes and clients.
- Credit balance display: attach an Orb portal link to an account and see used and remaining credit, the reset date and a usage bar on the accounts page, with a "Refresh Orb balance" action.
- Model list page that lists every model's reasoning levels and context length, with a "Refresh catalogue" action that pulls from Zed on demand.
- Selectable probe model: probe every account with any model from the catalogue, and the accounts page remembers the choice.
- Gateway API key authentication with a fail-closed bind rule: a non-loopback bind without any configured key refuses to start.
- Prebuilt multi-arch image on GHCR, plus compose files for the prebuilt image and for building from source.
- Paste-callback login for container and remote deployments, plus a `login` command for hosts.
- `/health` liveness endpoint and a `healthcheck` subcommand for container orchestration.
- Rust toolchain: one static musl binary per architecture, with the Web UI embedded at compile time.

## Web UI

The gateway panel at `/` is served by the gateway itself. The dark side rail carries the service state (running, model count, address), the five-entry navigation and two short notes; the page area shows one of five pages:

- **Account pool**: the account list in scheduling order, with subscription plan, quota usage, Orb balance and the latest probe for each account. A probe model selector and three buttons sit above the list: "Refresh account status" re-reads tokens and subscription plans without calling a model, "Refresh Orb balance" re-reads Orb only, and "Probe all accounts" sends one short request per account. A banner under the page header says which model and how many output tokens a probe uses.
- **Model list**: the forwarded catalogue with each model's provider, default and supported reasoning levels, and context length. "Refresh catalogue" pulls it from Zed on demand; "Use for probes" picks the model the accounts page probes with.
- **Diagnostics**: six read-only checks in a fixed order (service, accounts configured, token validity, and the three protocol routes), run once when the panel loads, with a pass/fail state and a finish time.
- **Routes**: the three client protocols, then every route the gateway serves in six static groups, each row with the method and whether a gateway key is needed. The page sends no requests.
- **Clients**: base-address notes and four copy-ready snippets (Codex, Claude Code, OpenCode, a shell request), built from the address you are reading the page on, with a placeholder key and a model from the catalogue.

All five pages exist in both languages; the switch in the top bar changes the language in place and keeps the current page. Every data view has an empty, loading and error state, a failed refresh keeps the values already on screen, and a failure on one page does not affect the others.

<p align="center"><img src="docs/assets/webui-health.en.png" alt="ZedProxy gateway panel, diagnostics page" width="760"></p>

<p align="center"><sub>The diagnostics page: six read-only checks, no inference request is sent. Screenshot from the fake-data test gateway.</sub></p>

## Quick start

```sh
cp .env.example .env          # then set ZED_API_KEY to your own value
docker compose up -d          # pulls ghcr.io/wray-lee/zed-proxy
curl -s http://127.0.0.1:8001/health
```

1. **Add a Zed account.** Open `http://127.0.0.1:8001/` and use the login button, or see [Login](docs/login.md) for the paste-callback flow used in containers.
2. **Point a client at it.** Use `http://127.0.0.1:8001/v1` as the base URL and your `ZED_API_KEY` as the key.

```sh
curl http://127.0.0.1:8001/v1/chat/completions   -H "Authorization: Bearer $ZED_API_KEY" -H "Content-Type: application/json"   -d '{"model":"gpt-5.6","messages":[{"role":"user","content":"Hello"}]}'
```

`docker-compose.yml` runs the prebuilt `linux/amd64` and `linux/arm64` image from GHCR. To build from this checkout instead, run `docker compose -f docker-compose.build.yml up -d --build`. Both publish only `127.0.0.1:8001` and keep data in `./data`; on Linux run `chown -R 10001:10001 ./data` first.

To build the binary directly you need Rust 1.99 and Node.js 22 (for the embedded Web UI):

```sh
cd webui && npm ci && npm run build && cd ..
cargo build --release          # target/release/zed-proxy
```

The binary embeds `webui/dist/index.html`, so the Web UI build must run first; `build.rs` fails the build with an explicit message when that file is missing. See [Development](docs/development.md) for `cargo fmt`, `clippy`, `test` and the musl image.

## Supported protocols

| Client format | Route |
| --- | --- |
| OpenAI Chat Completions | `POST /v1/chat/completions` |
| OpenAI Responses | `POST /v1/responses` |
| Anthropic Messages | `POST /v1/messages` |

Streaming and non-streaming are supported on all three, and each route can route to every provider. Models come from Zed's catalogue, all of them, served as listed (GPT, Claude and Gemini families); the full route list is in [API endpoints](docs/api.md).

## Documentation

| Page | What it covers |
| --- | --- |
| [Authentication](docs/authentication.md) | Gateway keys, protected paths, bind policy |
| [Login](docs/login.md) | Adding Zed accounts: paste callback, command line, local callback |
| [Client setup](docs/clients.md) | OmniRoute, Claude Code and other clients |
| [API endpoints](docs/api.md) | Every route and its gateway-key requirement |
| [Configuration](docs/configuration.md) | Environment variables, command line and data files |
| [Architecture](docs/architecture.md) | Modules, request flow, login flow, layering |
| [Known limitations](docs/limitations.md) | What to expect and what is not verified |
| [Terms and risk](docs/terms-risk.md) | Unofficial status and Zed's Terms of Service |
| [Development](docs/development.md) | Building and testing from source |
| [Gateway panel (WebUI)](docs/webui.md) | Panel files, design rules and behaviour |

## Acknowledgements

- [csmsqj/zed-api](https://github.com/csmsqj/zed-api) was the starting point of this project and showed how Zed's endpoints are called. From 0.3.0 the gateway is a clean-room rewrite in Rust and contains none of its code. Releases before 0.3.0 were derived from it and carry its MIT copyright notice in their git history.

## License

MIT, see [LICENSE](LICENSE). Contributions are welcome, see [CONTRIBUTING.md](CONTRIBUTING.md); security reports go through [SECURITY.md](SECURITY.md).
