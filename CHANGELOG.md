# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog v1.1.0](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.3.0] - 2026-11-08

The Zig implementation is replaced by a clean-room Rust rewrite. The public
HTTP contract is preserved; the changes below are the ones a user can notice.

The Rust code and the rebuilt WebUI share no code with csmsqj/zed-api (whole-tree
line overlap 0.4%, protocol boilerplate excluded), so the licence now carries
only the project's own copyright; csmsqj/zed-api stays credited in the README.

### Added

- `/v1/models` entries now carry `context_length`, `max_output_tokens` and
  `max_completion_tokens` when the catalogue has them, and the `gpt-5.6` alias
  copies `gpt-5.6-sol`'s values. The Codex `models[]` list is unchanged.
- `POST /zed/models/refresh` pulls the catalogue on demand with a 5-second
  minimum interval and returns `{"refreshed":true,"count":N}`,
  `{"refreshed":false,"retry_after_s":s,"count":N}` when throttled, or 502
  when the upstream fails (the old catalogue is kept).
- `POST /zed/accounts/orb/refresh` force-refreshes the Orb credit balance for
  one account or all of them, bypassing the 5-minute/60-second cache with a
  5-second per-account floor, and keeps the last good numbers when the portal
  fails. Responses use the fixed-decimal `credit` object.
- The accounts page has "Refresh account status" and "Refresh Orb balance"
  buttons and a "Probe all accounts" button, and the models page has "Refresh
  catalogue"; all are bilingual, show a busy state, report success or failure
  and keep the values already on screen when a refresh fails. The probe model
  selector above the account list chooses the model every probe uses.
- `zed-proxy healthcheck [--url <url>]` replaces the container's `curl` health
  check.
- `ZED_UPSTREAM_BASE` overrides the base URL of the four Zed endpoints (for
  tests and self-hosting against a mirror).
- Explicit neutral request/response layer between the three client protocol
  codecs and the three provider encoders.

### Changed

- Rust 1.99, static musl binaries for `linux/amd64` and `linux/arm64`, with
  `panic = "abort"`, LTO, `opt-level = "s"` and stripped output.
- All outbound HTTP, TLS, streaming and the login RSA run in-process; the
  runtime image contains neither `curl` nor `openssl`.
- `/v1/responses` can now route to Google models, so every client protocol can
  reach every provider.
- Gemini tool schemas are cleaned to the Gemini subset before being sent, and
  tool calls round-trip in both directions.
- Error handling is unified across streaming and non-streaming: request-level
  failures (upstream 400/404/405/413/415/422, including `status.failed`
  wrappers on HTTP 200) return 400 with the upstream message and are never
  retried, cooled down, or failed over. Final statuses follow the last error
  kind (400/401/403/429/502) instead of a generic 502.
- Anthropic errors use the Anthropic error body on `/v1/messages`; Chat and
  Responses use the OpenAI shape.
- Only Linux and containers are supported: the Windows registry proxy lookup,
  the BCrypt login branch, the Winsock wrappers and the Windows link
  configuration are gone.
- `GET /` sends `Cache-Control: no-store`.
- The Web UI is a clean-room rewrite in TypeScript, built by Vite into one
  self-contained `webui/dist/index.html` and embedded at compile time. The
  layout is new: a dark side rail with the service state and a numbered
  navigation, two core colours (paper and ink) plus one terracotta accent,
  serif headings over sans body text, hairline-separated rows instead of a card
  grid, and a 4/8/12 px radius scale. The five pages are Account pool, Model
  list, Diagnostics, Routes and Clients; the browser's language is used
  unless `zed_lang` says otherwise, and the switch in the top bar changes the
  language in place on the current page.
- Every page renders in its own guard with an error surface and a retry: one
  failing page no longer stops the others, and the navigation and language
  switch stay usable. A browser that blocks site data gets the whole console
  instead of a blank page.
- Loading, empty and error states on every data view, state shown with an icon
  and a text label rather than colour alone, 44 px targets, visible focus
  rings, one dialog at a time, and a static wait under reduced motion.
- The Web UI documentation moves to [docs/webui.md](docs/webui.md) with a
  Chinese version, including the reason for each visual device.
- `configs/claude.settings.json.example` and `accounts.example.json` are
  removed; the documentation no longer references them.
- `GET /zed/usage` returns the active account's LLM-token JWT payload (claims
  only, never the full token); `GET /zed/billing` returns Zed's raw
  `client/users/me` body unchanged. Both answer `400 {"error":"no account"}`
  without an active account; token failures are `500 {"error":"internal
  error"}` and upstream failures `502 {"error":"failed to fetch user info"}`.
- Gemini requests now carry `generationConfig.maxOutputTokens`, computed as
  `min(max(client value, 32), catalogue max_output_tokens)` (catalogue missing:
  1048576), or the catalogue value when the client sends none. The health probe
  reports the same number instead of `null`.

### Fixed

- The Web UI no longer renders a blank console when the browser blocks site
  data. The i18n module read `localStorage` while initializing; the uncaught
  `SecurityError` stopped the whole entry script, so no navigation or page
  container rendered (reported as the client setup and health pages
  disappearing). Every `localStorage` access now falls back to an
  in-memory/default value. Evidence in `webui-check/rust-0.3.0/`: fake-gateway
  probe `tools/localstorage-blocked.js` (0.2.2 `dist`: `nav:0`, `appLen:0`,
  `EXC SecurityError`; 0.3.0 `dist`: five non-empty pages, no errors) plus
  `accounts.{en,zh}.png`, `health.{en,zh}.png`, `buttons.{en,zh}.log`.
- `POST /zed/accounts/switch` with a non-object body (for example `[]`) no
  longer crashes the process; it returns 400.
- An `accounts.json` whose root is an array, or whose `accounts` member is not
  an object, is read as zero accounts instead of crashing.
- Account state (cooldown and consecutive failures) is updated atomically, and
  `accounts.json` writes are atomic (temp file + rename at `0600`).
- The attempt order has no 64-account limit; every account is tried once.
- Login no longer writes ciphertext or private key material to disk, and the
  RSA keypair is generated and used in-process.
- Streaming no longer drops upstream lines when one network chunk contains
  several events.
- A successful probe no longer wipes the account's Orb balance, its link state
  or its billing flags: the merge applies only the fields a probe response
  actually carries, and a probe without a token verdict leaves the earlier one
  alone.
- Throttled refreshes now say so in place. A balance refresh that returns
  `retry_after_s` reports the wait alongside any successes, and the catalogue
  refresh reports its wait seconds instead of a generic failure.
- A rejected login shows its error inline at the paste field, keeps the pasted
  value, and gives an actionable line instead of a raw status code.
- Choosing a language in the top bar applies to the current session even when
  site data is blocked: the pages are redrawn in place rather than reloaded, so
  no fourth storage key is added.
- Account actions carry the account name in their accessible name while keeping
  the visible label first (`Probe this account` followed by the name), so voice
  and screen-reader users can tell the cards apart.
- The account card reports Zed's overdue-bill and account-too-young flags from
  the status response next to the period end date, and shows nothing when the
  server does not set them.
- `webui/src/theme.css` declares each recurring property once on the role that
  owns it (text, hairline frame, machine text) instead of repeating it inside
  every component rule.
- Control and container boundaries use `#8a8375` (3.4:1 or better on paper and
  on cards), so input, select, secondary-button and card edges clear the 3:1
  component-contrast floor; the focus ring stays at 6.3:1.
- Models that only the upstream `data[]` list carries are read-only rows marked
  "Upstream list only" with no probe button, and the built-in-catalogue toast
  reports the count of the list it is showing instead of the refresh response.
- Login failures get one line per status class (409 no callback waiting, 410
  expired, 500 not saved, 400 names the callback problem), and a rejected Orb
  link spells out the accepted shape: https, `portal.withorb.com`, a non-empty
  token parameter.
- A probe that is cooling down shows the remaining seconds next to the probe
  result, and an unanswered write tells the reader the result may still have
  taken effect and points at "Refresh account status".

### Removed

- The Zig build (`build.zig`, `build.zig.zon`), the `zig-out` layout and the
  Zig source tree.
- The `curl`/`openssl` subprocess paths and the `zed-proxy_req_*.json` /
  `zed-proxy_stream_*.json` / `zed-proxy_enc_*.bin` temporary files.

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
