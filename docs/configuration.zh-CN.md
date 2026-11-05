[← 返回 README](../README.zh-CN.md)

# 配置

配置在启动时从环境读取一次；进程运行期间修改环境变量不会生效。

| 变量 | 默认值 | 含义 |
| --- | --- | --- |
| `ZED_API_HOST` | `127.0.0.1` | 监听地址。`localhost` 会按 `127.0.0.1` 处理；其它值必须是 IP 字面量（`0.0.0.0`、`::`、`192.0.2.10` 等），除 `localhost` 外的域名在启动时被拒绝。空值按默认值处理。非回环地址要求至少配置一个网关 key。 |
| `ZED_API_PORT` | `8001` | 监听端口（1–65535）。非法值退回默认。位置参数端口（`zed-proxy serve 9000`）优先。 |
| `ZED_DATA_DIR` | 未设（当前目录） | `accounts.json`、`active_account.txt`、`api_key` 所在目录。不存在时自动创建（含父目录），进程在加载状态前切到该目录。目录非法或不可写时启动以非 0 退出。 |
| `ZED_API_KEY` | 未设 | 网关 API key，逗号或换行分隔；每项去空白，空项丢弃。优先级最高。 |
| `ZED_API_KEY_FILE` | 未设 | 存放网关 API key 的文件，格式同上。读取失败（不存在、不可读、超过 1 MiB）视为没有 key，且不再回退到下一个来源。 |
| `ZED_LOGIN_PORT` | `58443` | 拼接进粘贴登录授权链接的回调端口。该模式下没有进程监听它。空值取默认；非法值使登录接口返回 400。 |
| `ZED_SYSTEM_ID` | 未设 | 换取 LLM 令牌时发送的 `x-zed-system-id`。未设或空串则不发送该头。 |
| `ZED_UPSTREAM_BASE` | `https://cloud.zed.dev` | 四个 Zed 端点（`completions`、`client/llm_tokens`、`models`、`client/users/me`）的基址。必须是 `http(s)://host[:port][/prefix]`；去掉结尾 `/` 后作为前缀。其它值导致启动失败。它不影响 `zed.dev`（登录页）。 |
| `ZED_ORB_BASE` | 未设 | 仅调试构建：覆盖 Orb 门户基址（`https://portal.withorb.com`），让余额功能的测试钩子不访问真实门户。release 二进制忽略它，始终使用固定的门户基址。它不是受支持的部署配置。 |
| `HTTPS_PROXY`、`https_proxy`、`ALL_PROXY`、`all_proxy` | 未设 | 所有出站请求使用的 HTTP 代理，按该顺序尝试；空值与不可解析值跳过。URI 未带端口时用 `7890`。只解析一次并缓存。 |
| `RUST_LOG` | 未设 | 预留：当前构建以固定前缀写 stderr，尚未读取日志过滤表达式。 |

镜像设置了 `ZED_API_HOST=0.0.0.0`、`ZED_API_PORT=8001`、`ZED_LOGIN_PORT=58443`、`ZED_DATA_DIR=/data`。compose 从 `.env` 传入 `ZED_API_KEY`。

不存在 Windows 系统代理读取：代理只来自上面的环境变量。

## 命令行

```
zed-proxy [command] [args...]
```

不带子命令时等价于 `serve`。

| 命令 | 行为 |
| --- | --- |
| `serve [port]` | 启动 HTTP 服务并阻塞运行。端口优先级：位置参数 > `ZED_API_PORT` > `8001`；位置参数非法时直接退回 `8001`，不再看环境变量。启动时打印 `[zed-proxy] http://<host>:<port>`、`<n> account(s) loaded`、`auth: on\|off`，以及 `proxy: <host>:<port>` 或 `proxy: none (set HTTPS_PROXY to use)`。监听失败以 1 退出。 |
| `login [name]` | 走本地回调登录，把账号保存为 `name`（省略时用 `user_id`），最多等待 600 秒。 |
| `accounts` | 逐行打印 `  <name> (uid: <user_id>)`。不访问网络。没有账号时打印 `No accounts. Run: zed-proxy login`。 |
| `healthcheck [--url <url>]` | 以 3 秒预算 GET `http://127.0.0.1:<ZED_API_PORT 或 8001>/health`（或 `--url`）；仅当 HTTP 200 且响应体为 `{"status":"ok"}` 时退出码 0，否则在 stderr 打印原因并以 1 退出。镜像的 `HEALTHCHECK` 使用它，不需要网关 key。 |

## 数据目录与文件

数据目录在 `ZED_DATA_DIR` 设置且非空时取它，否则取启动时的工作目录。所有相对路径都相对于它解析，因为进程已切换进去。

| 文件 | 内容 | 权限 |
| --- | --- | --- |
| `accounts.json` | `{"accounts":{"<名称>":{"user_id":"…","credential":{…},"orb_url":"…"}}}`；`orb_url` 可缺省。账号名与 `user_id` 上限 256 字节，文件上限 4 MiB，每次写入都经过同目录 `0600` 临时文件加 `rename`。 | `0600` |
| `active_account.txt` | 当前账号名。 | `0600` |
| `api_key` | 网关 key 文件，仅在两个环境变量都未设置时使用。 | 由用户决定 |
| `.write_probe` | 登录前的可写性检查；创建后立即删除。 | `0600` |

登录、Orb 链接变更与保存失败只报错，不会留下写了一半的文件。

## `accounts.json`

每个账号条目包含 `user_id`、Zed 的 `credential` JSON，以及可选的 `orb_url`：

| 字段 | 含义 |
| --- | --- |
| `orb_url` | 用于余额显示的 Orb portal 链接（`https://portal.withorb.com/?token=<orb-token>`）。由 `POST /zed/accounts/orb` 写入；登录新增或重写账号时会保留。 |

`orb_url` 等价于一份 bearer 凭据：拿到该链接的人可以读取对应客户的 Orb 数据。它以明文存放在 `accounts.json` 中，POSIX 上该文件以 `0600` 创建；API 不会返回它，日志也不会写入它。见 [SECURITY.md](../SECURITY.md)。
