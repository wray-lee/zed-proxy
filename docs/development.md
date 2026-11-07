[← Back to README](../README.md)

# Development

See [CONTRIBUTING.md](../CONTRIBUTING.md) for contribution rules.

```sh
cd webui && npm ci && npm run build && cd ..   # the binary embeds webui/dist/index.html
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --release                           # target/release/zed-proxy
docker build -t zed-proxy:local .
```

Built and tested with Rust 1.99; the crate's MSRV is 1.88 (edition 2021; `icu_provider` 2.4 in the lockfile needs it). The Web UI must be built once with `npm ci && npm run build` inside `webui/` before `cargo build`, because `src/http/static.rs` embeds `webui/dist/index.html` with `include_bytes!` and `build.rs` fails the build when the file is missing. The Web UI needs Node.js 22.

Only Linux and containers are supported. The release build is a static musl binary:

```sh
rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl
cargo build --release --locked --target x86_64-unknown-linux-musl
cargo build --release --locked --target aarch64-unknown-linux-musl
```

The Dockerfile builds both architectures natively under QEMU (the `build` stage is `rust:1.99-alpine`, which is musl), and the runtime image contains neither `curl` nor `openssl`: health checks use `zed-proxy healthcheck`. The image runs as uid 10001 with `/data` at `0700`.

Tests live in each module's `#[cfg(test)] mod tests`; there is no `tests/` directory. The panel in `webui/` is plain TypeScript with no UI framework; it type-checks with `npx tsc --noEmit` and builds with `npm run build`, and the bundle is a single `dist/index.html` (Vite plus `vite-plugin-singlefile`). Its structure and design rules are in [Gateway panel (WebUI)](webui.md).
