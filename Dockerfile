# syntax=docker/dockerfile:1

# ---- Web UI: build the single-file HTML bundle ----
FROM --platform=$BUILDPLATFORM node:22-alpine AS webui
WORKDIR /src/webui
COPY webui/package.json webui/package-lock.json ./
RUN npm ci
COPY webui/ ./
RUN npm run build

# ---- Server: static musl binary ----
# Builds run for the target platform: on an amd64 host the linux/arm64 image is
# compiled under QEMU (CI registers it with setup-qemu-action). rust:alpine
# already ships gcc and musl-dev, which ring's C code needs.
FROM rust:1.99-alpine AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock build.rs ./
COPY src ./src
# build.rs and include_bytes! need the bundle before cargo starts.
COPY --from=webui /src/webui/dist ./webui/dist
ARG TARGETARCH
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target,id=cargo-target-${TARGETARCH} \
    set -eu; \
    case "$TARGETARCH" in \
      amd64) target=x86_64-unknown-linux-musl ;; \
      arm64) target=aarch64-unknown-linux-musl ;; \
      *) echo "unsupported TARGETARCH: $TARGETARCH" >&2; exit 1 ;; \
    esac; \
    cargo build --release --locked --target "$target"; \
    mkdir -p /out; \
    cp "target/$target/release/zed-proxy" /out/zed-proxy

# ---- Runtime ----
# ca-certificates only: the binary links TLS in-process and health-checks
# itself, so neither curl nor openssl is installed (spec 90 §5.2).
FROM alpine:3.22
RUN apk add --no-cache ca-certificates \
 && adduser -D -H -u 10001 zed \
 && mkdir -p /data \
 && chown zed:zed /data \
 && chmod 700 /data
COPY --from=build /out/zed-proxy /usr/local/bin/zed-proxy
USER zed
WORKDIR /data
ENV ZED_API_HOST=0.0.0.0 \
    ZED_API_PORT=8001 \
    ZED_LOGIN_PORT=58443 \
    ZED_DATA_DIR=/data
EXPOSE 8001
HEALTHCHECK --interval=30s --timeout=3s --start-period=5s --retries=3 \
  CMD ["/usr/local/bin/zed-proxy", "healthcheck"]
ENTRYPOINT ["/usr/local/bin/zed-proxy", "serve"]
