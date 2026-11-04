# Security Policy

## Supported versions

| Version | Supported |
| --- | --- |
| 0.1.x | Yes |

Only the latest 0.1.x release receives fixes. Older builds have not been
assessed and are not supported.

## Reporting a vulnerability

Report vulnerabilities through GitHub private vulnerability reporting:

https://github.com/wray-lee/zed-proxy/security/advisories/new

Do not open a public issue, pull request or discussion for a suspected
vulnerability. Please include the version or commit, the deployment shape
(binary or Docker, loopback or public bind), reproduction steps and any log
lines with credentials removed.

## Threat model

ZedProxy is a single-user gateway that holds Zed account credentials and
forwards requests on the user's behalf. It is not a multi-tenant service.

- **Access control.** The gateway API key is the only access control for the
  `/v1/` and `/zed/` path prefixes. `/health` and `/` are public and disclose
  no account data. `/api/event_logging/batch` is also answered without a key
  because it sits outside those prefixes; it acknowledges client event batches
  and stores nothing. Browser `OPTIONS` preflight is answered without a key.
- **Bind policy.** A non-loopback listen address with no configured key is
  refused at startup. This is fail-closed: the process exits instead of
  serving an unauthenticated proxy.
- **Credential storage.** Zed account credentials are stored in plaintext in
  `accounts.json` in the data directory (`/data` in the container). They are
  not encrypted at rest. The file is created `0600` on POSIX and the container
  data directory is `0700`; a host bind mount keeps its own mode, so run
  `chmod 700 ./data` as well as the `chown`. Do not commit `accounts.json`,
  back it up to a public location, or mount it where other workloads can read
  it. In the container the process runs as uid 10001, so the host directory can
  be owned by that uid.
- **No rate limiting or tenant isolation.** There is no quota, throttling or
  per-client separation. Every caller that holds a key shares the same account
  pool and the same upstream quota. Do not expose the gateway to untrusted
  users, and prefer a loopback bind unless a key is configured and the network
  is trusted.
- **Logging.** The server writes to stderr through `std.debug.print`, and the
  container runtime collects that stream. It does not print gateway keys or
  account credentials. Upstream error response bodies are logged for
  diagnosis, so review logs before sharing them.
- **Request limits.** Request bodies are capped at 16 MiB.

## Known limitations

- Upstream credentials are passed to `curl` as `-H` arguments (both the
  streaming and non-streaming paths, and the token/`users/me` requests), so
  they appear in the process argument list and can be read by other users on
  the same host. In the container all processes run under a single uid, which
  makes this harder to reach, but a shared host with untrusted local users
  widens the exposure. Fixing this needs `--config`/stdin header passing, which
  is out of scope for this release.
- `accounts.json` and `active_account.txt` are created with mode `0600` on
  POSIX, and the container data directory is `0700`. Tell clients that a
  bind-mounted host directory keeps whatever mode it already had; run
  `chmod 700 ./data` after `chown`.
- The login flow writes a short-lived ciphertext to a `0600` temp file in the
  data directory while `openssl` decrypts it.
