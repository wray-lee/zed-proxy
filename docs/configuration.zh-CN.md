[← 返回 README](../README.zh-CN.md)

# 配置

| 变量 | 默认值 | 含义 |
| --- | --- | --- |
| `ZED_API_HOST` | `127.0.0.1` | 监听地址。`localhost` 会按 `127.0.0.1` 处理。非回环地址要求至少配置一个网关 key。 |
| `ZED_API_PORT` | `8001` | 监听端口。位置参数端口（`zed-proxy serve 9000`）优先。 |
| `ZED_DATA_DIR` | 未设（当前目录） | `accounts.json`、`active_account.txt`、`api_key` 所在目录。不存在时自动创建，进程在加载状态前切到该目录。 |
| `ZED_API_KEY` | 未设 | 网关 API key，逗号或换行分隔。 |
| `ZED_API_KEY_FILE` | 未设 | 存放网关 API key 的文件。 |
| `ZED_LOGIN_PORT` | `58443` | 拼接进粘贴登录授权链接的回调端口。 |
| `ZED_SYSTEM_ID` | 未设置 | 换取令牌时发送的 `x-zed-system-id`。不设置则不发送这个头。 |
| `HTTPS_PROXY`、`ALL_PROXY`（含小写形式） | 未设 | 上游请求使用的 HTTP 代理。未设时 Windows 上读取系统代理。 |

镜像设置了 `ZED_API_HOST=0.0.0.0`、`ZED_API_PORT=8001`、`ZED_LOGIN_PORT=58443`、`ZED_DATA_DIR=/data`。compose 从 `.env` 传入 `ZED_API_KEY`。

## `accounts.json`

每个账号条目包含 `user_id`、Zed 的 `credential` JSON，以及可选的 `orb_url`：

| 字段 | 含义 |
| --- | --- |
| `orb_url` | 用于余额显示的 Orb portal 链接（`https://portal.withorb.com/?token=<orb-token>`）。由 `POST /zed/accounts/orb` 写入；登录新增或重写账号时会保留。 |

`orb_url` 等价于一份 bearer 凭据：拿到该链接的人可以读取对应客户的 Orb 数据。它以明文存放在 `accounts.json` 中，POSIX 上该文件以 `0600` 创建；API 不会返回它，日志也不会写入它。见 [SECURITY.md](../SECURITY.md)。
