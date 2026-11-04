[← 返回 README](../README.zh-CN.md)

# 开发

贡献规则见 [CONTRIBUTING.md](../CONTRIBUTING.md)。

```sh
zig build          # 生成 zig-out/bin/zed-proxy
zig build test     # 运行单元测试
docker build -t zed-proxy:local .
```

需要 Zig 0.15.2 或更新版本。`zig build` 之前必须先在 `webui/` 里执行一次 `npm ci && npm run build`，因为二进制会嵌入 `webui/dist/index.html`。
