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
