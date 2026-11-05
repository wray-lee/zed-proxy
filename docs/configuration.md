[← Back to README](../README.md)

# Configuration

Configuration is read once at startup from the environment; changing an environment variable while the process runs has no effect.

| Variable | Default | Meaning |
| --- | --- | --- |
| `ZED_API_HOST` | `127.0.0.1` | Bind address. `localhost` is accepted as an alias for `127.0.0.1`; any other value must be an IP literal (`0.0.0.0`, `::`, `192.0.2.10`, …), and hostnames other than `localhost` are rejected at startup. An empty value means the default. A non-loopback address requires at least one gateway key. |
| `ZED_API_PORT` | `8001` | Bind port (1–65535). An invalid value falls back to the default. A positional port (`zed-proxy serve 9000`) takes precedence. |
| `ZED_DATA_DIR` | unset (current directory) | Directory for `accounts.json`, `active_account.txt`, and `api_key`. Created (including parents) if missing, and the process changes into it before loading state. An invalid or unwritable directory makes startup exit non-zero. |
| `ZED_API_KEY` | unset | Gateway API keys, comma or newline separated; items are trimmed and empty items dropped. Highest priority. |
| `ZED_API_KEY_FILE` | unset | File containing gateway API keys, same format. A read failure (missing, unreadable, above 1 MiB) yields no keys and does not fall through to the next source. |
| `ZED_LOGIN_PORT` | `58443` | Callback port embedded in the paste login authorization URL. Nothing listens on it in that mode. Empty means the default; an invalid value makes the login endpoint answer 400. |
| `ZED_SYSTEM_ID` | unset | `x-zed-system-id` sent when minting LLM tokens. Unset or empty means the header is omitted. |
| `ZED_UPSTREAM_BASE` | `https://cloud.zed.dev` | Base URL for the four Zed endpoints (`completions`, `client/llm_tokens`, `models`, `client/users/me`). Must be `http(s)://host[:port][/prefix]`; a trailing `/` is stripped and the value becomes the prefix. Anything else fails startup. It does not affect `zed.dev` (the login page). |
| `HTTPS_PROXY`, `https_proxy`, `ALL_PROXY`, `all_proxy` | unset | Outbound HTTP proxy for every outbound request. Tried in that order; empty and unparseable values are skipped. A URI without a port uses `7890`. Parsed once and cached. |
| `RUST_LOG` | unset | Reserved: the current build writes fixed-prefix lines to stderr and does not read a log filter yet. |

The image sets `ZED_API_HOST=0.0.0.0`, `ZED_API_PORT=8001`, `ZED_LOGIN_PORT=58443`, and `ZED_DATA_DIR=/data`. The compose file passes `ZED_API_KEY` from `.env`.

There is no Windows system-proxy lookup: the proxy comes only from the variables above.

## Command line

```
zed-proxy [command] [args...]
```

With no command it behaves like `serve`.

| Command | Behaviour |
| --- | --- |
| `serve [port]` | Starts the HTTP service and blocks. Port priority: positional argument > `ZED_API_PORT` > `8001`; an invalid positional argument falls back to `8001` without consulting the environment. Prints `[zed-proxy] http://<host>:<port>`, `<n> account(s) loaded`, `auth: on\|off` and `proxy: <host>:<port>` or `proxy: none (set HTTPS_PROXY to use)`. A bind failure exits 1. |
| `login [name]` | Runs the local-callback login, saving the account under `name` (or the `user_id` when omitted), waiting up to 600 seconds. |
| `accounts` | Prints `  <name> (uid: <user_id>)` for every account. No network access. With no accounts it prints `No accounts. Run: zed-proxy login`. |
| `healthcheck [--url <url>]` | GETs `http://127.0.0.1:<ZED_API_PORT or 8001>/health` (or `--url`) with a 3-second budget; exit 0 only for HTTP 200 with `{"status":"ok"}`, otherwise exit 1 with a reason on stderr. Used by the image `HEALTHCHECK`; needs no gateway key. |

## Data directory and files

The data directory is `ZED_DATA_DIR` when set and non-empty, otherwise the working directory at startup. Every relative path resolves against it, because the process changes into it.

| File | Contents | Mode |
| --- | --- | --- |
| `accounts.json` | `{"accounts":{"<name>":{"user_id":"…","credential":{…},"orb_url":"…"}}}`; `orb_url` is optional. Account names and `user_id` are capped at 256 bytes, the file at 4 MiB, and every write goes through a same-directory `0600` temp file plus `rename`. | `0600` |
| `active_account.txt` | Active account name. | `0600` |
| `api_key` | Gateway key file, used when neither environment variable is set. | user's choice |
| `.write_probe` | Writable check before a login; created and immediately removed. | `0600` |

Login, Orb link changes and a failed save only report an error; they never leave a half-written file.

## `accounts.json`

Each account entry holds `user_id`, the Zed `credential` JSON, and an optional `orb_url`:

| Field | Meaning |
| --- | --- |
| `orb_url` | Orb portal link (`https://portal.withorb.com/?token=<orb-token>`) used for the credit balance display. Written by `POST /zed/accounts/orb`; login keeps it when it adds or rewrites accounts. |

`orb_url` is a bearer credential: anyone who has the link can read that customer's Orb data. It is stored in plaintext in `accounts.json`, which is created `0600` on POSIX, and it is never returned by the API or written to a log. See [SECURITY.md](../SECURITY.md).
