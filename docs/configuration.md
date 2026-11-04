[← Back to README](../README.md)

# Configuration

| Variable | Default | Meaning |
| --- | --- | --- |
| `ZED_API_HOST` | `127.0.0.1` | Bind address. `localhost` is accepted as an alias for `127.0.0.1`. A non-loopback address requires at least one gateway key. |
| `ZED_API_PORT` | `8001` | Bind port. A positional port (`zed-proxy serve 9000`) takes precedence. |
| `ZED_DATA_DIR` | unset (current directory) | Directory for `accounts.json`, `active_account.txt`, and `api_key`. It is created if missing and the process changes into it before loading state. |
| `ZED_API_KEY` | unset | Gateway API keys, comma or newline separated. |
| `ZED_API_KEY_FILE` | unset | File containing gateway API keys. |
| `ZED_LOGIN_PORT` | `58443` | Callback port embedded in the paste login authorization URL. |
| `ZED_SYSTEM_ID` | not set | `x-zed-system-id` sent when minting LLM tokens. Unset means the header is omitted. |
| `HTTPS_PROXY`, `ALL_PROXY` (also lowercase) | unset | Outbound HTTP proxy for upstream calls. When unset, the Windows system proxy is used on Windows. |

The image sets `ZED_API_HOST=0.0.0.0`, `ZED_API_PORT=8001`, `ZED_LOGIN_PORT=58443`, and `ZED_DATA_DIR=/data`. The compose file passes `ZED_API_KEY` from `.env`.
