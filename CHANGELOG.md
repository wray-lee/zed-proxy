# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog v1.1.0](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

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
