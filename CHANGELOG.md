# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog v1.1.0](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.2.2] - 2026-11-05

### Fixed

- Gemini requests no longer fail with `502 All accounts failed` when the client
  sends a `system` or `developer` message. Zed's Google parser only accepts the
  roles `user` and `model` (`unknown variant \`system\``), so that text now goes
  into `systemInstruction`, and tool-result turns are sent as `user` turns.
- Gemini request bodies no longer contain a stray comma when the first message
  is skipped or a content block has no text (images, `tool_use`), which was
  invalid JSON.

## [0.2.1] - 2026-11-05

### Fixed

- `/v1/models`, the Models page and the live-check selector now serve Zed's whole
  catalogue (23 models) instead of a hardcoded list of seven. Only models Zed
  marks disabled are left out.
- A requested slug that Zed lists is forwarded unchanged. Previously every
  `claude*` name became `claude-sonnet-5` and every other `gpt-*` name became
  `gpt-5.6-sol`; now only names Zed does not serve fall back that way.
- The Anthropic `max_tokens` cap follows the model's own `max_output_tokens`
  from the catalogue, and `claude-haiku-5*` drops sampling parameters like Sonnet 5.
- The embedded fallback catalogue (used before the first fetch or when Zed is
  unreachable) now holds the full list too.

## [0.2.0] - 2026-11-05

### Added

- `POST /zed/accounts/health` accepts an optional `model` slug from
  `GET /v1/models`. Unknown slugs return `400 {"error":"unknown model"}`; the
  default remains `gpt-5.6-luna`. The probe is built for the chosen model and
  the response reports the model, the reasoning effort and the output budget
  actually used (`null` for Gemini, which carries no output cap).
- `Models` page in the Web UI, listing every model's display name, slug,
  vendor, description, supported reasoning levels and default level, with an
  action that selects a model for the live check.
- "Test model" select in the account center, persisted in `localStorage`
  under `zed_probe_model`, sent with every live check. Button labels and the
  info banner name the selected model instead of hard-coding Luna.
- Optional `orb_url` per account in `accounts.json`, kept across rewrites.
  `POST /zed/accounts/orb` sets or clears it after validating the scheme, the
  exact host (no userinfo, default port only) and the `token` query parameter.
  Saving rewrites `accounts.json` through a `0600` temp file and an atomic
  rename, so a failed write cannot truncate stored credentials.
- `GET /zed/accounts/status` entries gain `orb_configured` and, when
  configured, a `credit` object with balance, cap, used amount and percentage,
  period start, reset date and total balance. Results are cached for 5 minutes
  (60 seconds on failure).
- Account cards show the credit balance with a usage bar and reset date, plus
  a "Link balance" / "Unlink" flow. The link is never displayed back.

## [0.1.1] - 2026-11-05

### Fixed

- Token refresh failed with HTTP 400 ("Invalid JSON") because the request to
  `client/llm_tokens` had an empty body. It now sends the account's
  `organization_id`, taken from `client/users/me` (default organization, else
  the personal one, else the first).

## [0.1.0] - 2026-11-05

First public release.

### Added

- Gateway API key authentication for the `/v1/` and `/zed/` paths, read from
  `ZED_API_KEY`, `ZED_API_KEY_FILE`, or `<data dir>/api_key`, accepted as
  `Authorization: Bearer <key>` or `x-api-key: <key>`, and compared with a
  timing-safe hash.
- Fail-closed bind policy: a non-loopback `ZED_API_HOST` with no configured key
  refuses to start.
- Configurable bind address and data directory (`ZED_API_HOST`, `ZED_DATA_DIR`).
- `ZED_SYSTEM_ID` sets the `x-zed-system-id` sent when minting tokens. The
  header is omitted when it is unset; there is no built-in id.
- `GET /health` liveness endpoint, which requires no key and returns no account
  data.
- Gemini thinking support: client `reasoning_effort` maps to Gemini
  `thinkingLevel`, and upstream parts marked `thought` are split into thinking
  output instead of visible text.
- Paste-back login for container and remote deployments: start the login with
  `POST /zed/login` and `mode=paste`, submit the callback URL with
  `POST /zed/login/complete`, and poll `GET /zed/login/status`.
- Docker image: a multi-stage build that bundles the Web UI, runs as uid 10001,
  and ships a `HEALTHCHECK`. Tags `v*` publish a multi-arch image to
  `ghcr.io/wray-lee/zed-proxy`; `docker-compose.yml` runs it and
  `docker-compose.build.yml` builds from source.
- Web management page served at `/`, in English and Chinese (browser language by
  default, switchable in the top bar).

### Changed

- Renamed the project, binary, image tag and Web UI branding to `zed-proxy`.
- Docker builds select the Zig tarball from `BUILDARCH`, so a native arm64 host
  builds without an x86_64 compiler.

### Fixed

- Double free of `response_buf` when a token refresh failed, which aborted the
  process on any refresh error.
- Account names and user IDs are escaped before being written as JSON keys, and
  account fields are length-capped, so a malformed login can no longer corrupt
  `accounts.json`.
- Login uses a single slot, so a second concurrent login is rejected instead of
  racing the first.
- The legacy login callback wait is bounded by a deadline instead of blocking
  forever.
- An unwritable data directory now fails the login with a diagnostic response
  instead of a generic save error.
