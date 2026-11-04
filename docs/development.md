[← Back to README](../README.md)

# Development

See [CONTRIBUTING.md](../CONTRIBUTING.md) for contribution rules.

```sh
zig build          # build zig-out/bin/zed-proxy
zig build test     # run the unit tests
docker build -t zed-proxy:local .
```

Zig 0.15.2 or newer is required. The web page must be built once with `npm ci && npm run build` inside `webui/` before `zig build`, because the binary embeds `webui/dist/index.html`.
