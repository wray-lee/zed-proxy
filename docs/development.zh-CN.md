[← 返回 README](../README.zh-CN.md)

# 开发

贡献规则见 [CONTRIBUTING.md](../CONTRIBUTING.md)。

```sh
cd webui && npm ci && npm run build && cd ..   # 二进制会嵌入 webui/dist/index.html
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --release                           # target/release/zed-proxy
docker build -t zed-proxy:local .
```

使用 Rust 1.99 构建与测试；crate 的 MSRV 为 1.88（edition 2021；lockfile 里的 `icu_provider` 2.4 需要该版本）。`cargo build` 之前必须先在 `webui/` 里执行一次 `npm ci && npm run build`，因为 `src/http/static.rs` 用 `include_bytes!` 嵌入 `webui/dist/index.html`，该文件缺失时 `build.rs` 会让构建失败。Web UI 需要 Node.js 22。

仅支持 Linux 与容器。发布构建是静态 musl 二进制：

```sh
rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl
cargo build --release --locked --target x86_64-unknown-linux-musl
cargo build --release --locked --target aarch64-unknown-linux-musl
```

Dockerfile 在 QEMU 下为两种架构分别原生构建（`build` 阶段是 `rust:1.99-alpine`，本身即 musl），运行镜像里既没有 `curl` 也没有 `openssl`：健康检查用 `zed-proxy healthcheck`。镜像以 uid 10001 运行，`/data` 权限为 `0700`。

测试放在各模块的 `#[cfg(test)] mod tests` 中，没有 `tests/` 目录。`webui/` 里的控制台是纯 TypeScript、不引入 UI 框架，用 `npx tsc --noEmit` 做类型检查、`npm run build` 构建，产物是单个 `dist/index.html`（Vite 加 `vite-plugin-singlefile`）。其结构与设计规则见[控制台（WebUI）](webui.zh-CN.md)。
