# Contributing

Thanks for taking the time to contribute to ZedProxy.

## Prerequisites

- Zig 0.15.2 (the version pinned in `build.zig.zon`)
- Node.js 22 or newer (the Web UI build)
- Docker, only if you want to build the container image
- `curl` and `openssl` on Linux, for the login and upstream request paths

## Local build

The binary embeds `webui/dist/index.html`, so build the Web UI first. A fresh
clone therefore needs an `npm ci` before `zig build` can succeed.

```sh
cd webui && npm ci && npm run build
cd ..
zig build
zig build test
docker build -t zed-proxy:local .
```

`zig build` writes `zig-out/bin/zed-proxy`. `zig build test` runs the unit
tests. The Docker build repeats the Web UI and Zig steps inside the image and
is optional for a code-only change, but the pull request checklist asks you to
say whether you ran it.

## Commits

- Follow [Conventional Commits](https://www.conventionalcommits.org/). Use one
  of these prefixes: `feat`, `fix`, `docs`, `build`, `chore`, `refactor`.
- Write the subject in the imperative mood, for example `fix: bound legacy
  login wait` rather than `fixed` or `fixes`.
- One commit does one thing. Do not mix a rename, a behaviour change and a
  documentation update in the same commit.
- Set the author to `wray-lee <i@wray7.top>`. Do not add AI attribution
  trailers such as `Co-Authored-By` or `Generated-By`.

## Pull requests

- Run `zig build test` and report the result. Add a test for new behaviour.
- Do not commit credentials. `accounts.json`, `.env` files and the `data/`
  directory are ignored; keep them that way, and check logs and test fixtures
  for real tokens or keys before opening the pull request.
- Update the documentation in the same pull request when you change a
  behaviour, an endpoint, an environment variable or a default. That means
  `README.md`, `README.zh-CN.md` and `docs/architecture.md` where relevant,
  plus `CHANGELOG.md` for user-visible changes.
- Use LF line endings. `.gitattributes` normalises text files on commit; do
  not defeat it with `core.autocrlf` settings of your own.
- Keep the change focused. Unrelated refactoring or formatting belongs in its
  own pull request.

Security issues do not belong in a pull request or a public issue; see
[SECURITY.md](SECURITY.md).
