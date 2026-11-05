# Contributing

Thanks for taking the time to contribute to ZedProxy.

## Prerequisites

- Built and tested with Rust 1.99; the crate's MSRV is 1.88 (edition 2021; `icu_provider` 2.4 in the lockfile needs it). Only Linux and containers are supported; there is no Windows or macOS build.
- Node.js 22 or newer (the Web UI build)
- Docker, only if you want to build the container image

No `curl` or `openssl` is needed: upstream requests, TLS and the login RSA work all happen inside the process, and the container health check uses the `healthcheck` subcommand.

## Local build

The binary embeds `webui/dist/index.html`, so build the Web UI first. A fresh
clone therefore needs an `npm ci` before `cargo build` can succeed.

```sh
cd webui && npm ci && npx tsc --noEmit && npm run build && cd ..
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build                       # target/debug/zed-proxy
cargo build --release             # target/release/zed-proxy
docker build -t zed-proxy:local .
```

`cargo fmt --check` must report no diff, `cargo clippy --all-targets -- -D warnings` zero warnings, and `cargo test` must be green. Tests live in each module's `#[cfg(test)] mod tests`; do not add a `tests/` directory. `build.rs` fails the build when `webui/dist/index.html` is missing. The Docker build repeats the Web UI and Rust steps inside the image (building both `linux/amd64` and `linux/arm64` targets) and is optional for a code-only change, but the pull request checklist asks you to say whether you ran it.

## Documentation

Behaviour, endpoints, environment variables and defaults are documented in both languages: `README.md` and `README.zh-CN.md`, plus the matching pages under `docs/` (`api`, `architecture`, `authentication`, `clients`, `configuration`, `development`, `limitations`, `login`, `terms-risk`, each with a `.zh-CN.md` twin). Change one, change its twin in the same commit, and add an entry to `CHANGELOG.md` for user-visible changes. The two versions must stay equivalent; a reviewer will compare them.

## Commits

- Follow [Conventional Commits](https://www.conventionalcommits.org/). Use one
  of these prefixes: `feat`, `fix`, `docs`, `build`, `chore`, `refactor`.
- Write the subject in the imperative mood, for example `fix: bound legacy
  login wait` rather than `fixed` or `fixes`.
- One commit does one thing. Do not mix a rename, a behaviour change and a
  documentation update in the same commit.
- Set the author to `wray-lee <i@wray7.top>`. Do not add AI attribution
  trailers such as `Co-Authored-By` or `Generated-By`.
- Push over SSH (`git@github.com:wray-lee/zed-proxy.git`). A change that
  touches `.github/workflows/*` must be pushed over SSH, because the `gh` CLI
  token does not carry the `workflow` scope.

## Pull requests

- Run `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test` and the Web UI checks (`npx tsc --noEmit`, `npm run build`) and report the result. Add a test for new behaviour.
- Do not commit credentials. `accounts.json`, `.env` files and the `data/`
  directory are ignored; keep them that way, and check logs and test fixtures
  for real tokens or keys before opening the pull request.
- Update the documentation in the same pull request when you change a
  behaviour, an endpoint, an environment variable or a default. That means
  `README.md`, `README.zh-CN.md` and the matching pages under `docs/` (both
  languages) where relevant,
  plus `CHANGELOG.md` for user-visible changes.
- Use LF line endings. `.gitattributes` normalises text files on commit; do
  not defeat it with `core.autocrlf` settings of your own.
- Keep the change focused. Unrelated refactoring or formatting belongs in its
  own pull request.

Security issues do not belong in a pull request or a public issue; see
[SECURITY.md](SECURITY.md).
