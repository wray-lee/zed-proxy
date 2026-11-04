# syntax=docker/dockerfile:1

# ---- Web UI: build the single-file HTML bundle ----
FROM --platform=$BUILDPLATFORM node:22-alpine AS webui
WORKDIR /src/webui
COPY webui/package.json webui/package-lock.json ./
RUN npm ci
COPY webui/ ./
RUN npm run build

# ---- Server: cross-compile a static musl binary ----
# Fetch the compiler tarball from ziglang.org instead of relying on a
# prebuilt Zig container image.
FROM --platform=$BUILDPLATFORM alpine:3.22 AS build
ARG ZIG_VERSION=0.15.2
# BUILDPLATFORM is the machine doing the build; a native arm64 host needs the
# aarch64 compiler (x86_64 zig would fail with exec format error).
ARG BUILDARCH
RUN apk add --no-cache curl xz \
 && case "$BUILDARCH" in \
      amd64) ZA=x86_64 ;; \
      arm64) ZA=aarch64 ;; \
      *) echo "unsupported BUILDARCH: $BUILDARCH" >&2; exit 1 ;; \
    esac \
 && curl -fsSL "https://ziglang.org/download/${ZIG_VERSION}/zig-${ZA}-linux-${ZIG_VERSION}.tar.xz" -o /tmp/zig.tar.xz \
 && tar -xJf /tmp/zig.tar.xz -C /opt \
 && ln -s "/opt/zig-${ZA}-linux-${ZIG_VERSION}/zig" /usr/local/bin/zig \
 && rm /tmp/zig.tar.xz
WORKDIR /src
COPY build.zig build.zig.zon ./
COPY src ./src
COPY --from=webui /src/webui/dist ./webui/dist
ARG TARGETARCH
RUN case "$TARGETARCH" in \
      amd64) ZT=x86_64-linux-musl ;; \
      arm64) ZT=aarch64-linux-musl ;; \
      *) echo "unsupported TARGETARCH: $TARGETARCH" >&2; exit 1 ;; \
    esac \
 && zig build -Doptimize=ReleaseSafe -Dtarget="$ZT" --cache-dir /tmp/zc --global-cache-dir /tmp/zg \
 && mkdir -p /out \
 && cp zig-out/bin/zed-proxy /out/zed-proxy

# ---- Runtime ----
# curl is required by the streaming and proxy paths (spawned subprocesses) and
# by the healthcheck; openssl by Linux login/decrypt; ca-certificates by the
# std.http.Client TLS calls.
FROM alpine:3.22
RUN apk add --no-cache ca-certificates curl openssl \
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
  CMD curl -fsS "http://127.0.0.1:${ZED_API_PORT:-8001}/health" || exit 1
ENTRYPOINT ["/usr/local/bin/zed-proxy", "serve"]
