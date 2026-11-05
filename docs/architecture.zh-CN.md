[← 返回 README](../README.zh-CN.md)

# 架构

本页梳理模块，以及两条最重要的流程：模型请求与登录。端点表和配置表不在此重复，见 [API 端点](api.zh-CN.md)和[配置](configuration.zh-CN.md)。

## 模块

| 文件 | 职责 |
| --- | --- |
| `src/main.zig` | 入口。解析子命令（`serve`、`login`、`accounts`），应用 `ZED_DATA_DIR`，读取 `ZED_API_HOST` 和 `ZED_API_PORT`，启动服务。 |
| `src/server.zig` | HTTP 服务：每连接一个线程、请求解析、鉴权关卡、路由表、账号与登录处理、模型列表、内嵌网页。 |
| `src/gateway_auth.zig` | 网关 key 加载（`ZED_API_KEY`、`ZED_API_KEY_FILE`、`<data>/api_key`）、常量时间比较、`Authorization` 与 `x-api-key` 解析、受保护路径前缀、回环地址判断，以及 fail-closed 绑定策略。 |
| `src/providers.zig` | 把 OpenAI Chat、OpenAI Responses、Anthropic Messages 请求转换成 Zed 的 `completions` 载荷，并把响应转换回来。包含兜底的模型名表（Zed 目录里有的 slug 原样转发，只有未知名称才会被改写）、推理强度规则、GPT 输出预算下限，以及 Gemini 的 `thinkingLevel` 与 `thought` 处理。 |
| `src/proxy.zig` | 面向 `cloud.zed.dev/completions` 的上游传输；Zed 客户端头常量（`ZED_VERSION`、user agent、状态消息头、可选的 `ZED_SYSTEM_ID`）；代理发现（`HTTPS_PROXY`、`ALL_PROXY`、Windows 系统代理）；上游失败解析。 |
| `src/zed.zig` | 从 `cloud.zed.dev/client/llm_tokens` 换取令牌、拉取模型目录、从 `client/users/me` 拉取账单、解析 JWT 过期时间，以及把令牌、转换、传输、响应转换串起来的各协议入口。 |
| `src/stream.zig` | 流式链路。启动 `curl`，解析上游事件流，按客户端协议输出 SSE，同时负责流式请求的账号 failover 循环。 |
| `src/accounts.zig` | 账号池：`accounts.json` 的读写、当前账号持久化、冷却状态、尝试顺序与失败分类。 |
| `src/account_status.zig` | `client/users/me` 账单响应的无凭据摘要。 |
| `src/orb_quota.zig` | Orb portal 链接校验、token 提取、ledger 响应解析与汇总。纯函数，不发 HTTP。 |
| `src/auth.zig` | RSA 密钥对生成与 OAEP-SHA256 解密，Windows 用 BCrypt，其他平台用 `openssl` 命令行。也包含本地回调登录流程。 |
| `src/zed_login.zig` | 解析粘贴的回调 URL 或查询串，并提供单登录槽的 `SlotState`。纯函数，不涉及 socket 和文件。 |
| `src/socket.zig` | Windows 与 POSIX 两条路径的 socket 读写封装，以及 HTTP 响应写出。 |
| `build.zig` | 构建 `zed-proxy` 可执行文件，嵌入 `webui/dist/index.html`，面向 Windows 时链接系统库，并把所有测试模块注册到 `zig build test`。 |
| `webui/` | TypeScript 管理页，由 Vite 构建成单个 HTML 文件。 |

## 请求流程

1. `server.handleConnection` 读取请求行和请求头，并解析最大 16 MiB 的请求体，更大的返回 413。
2. 配置了任一网关 key 且路径以 `/v1/` 或 `/zed/` 开头时，请求必须带匹配的 key，否则在进入任何路由或上游处理之前返回 401。
3. `route` 按方法和路径分发。三个代理路由先校验 GPT-5.6 模型的客户端 `reasoning_effort`，再按 `stream` 标志分支。
4. 非流式：`handleProxy` 根据账号池生成尝试顺序，对每个账号调用 `zed.proxyChatCompletions`、`zed.proxyResponses` 或 `zed.proxyMessages`。它们各自获取或复用 LLM 令牌，用 `providers.buildZedPayload` 转换请求体，POST 到 `cloud.zed.dev/completions`，再把响应转换回来。遇到值得 failover 的错误时，当前账号进入冷却，改试下一个。
5. 流式：`stream.handleStreamProxy` 使用同样的尝试顺序，`doStreamProxy` 对同一端点运行 `curl -siN`，把每行上游事件翻译成客户端协议的 SSE。`stream_ended` 标记会结束响应并停止 `curl` 进程。
6. 上游失败被分类为鉴权、限流或瞬时错误，据此决定冷却时长。HTTP 200 上的 `status.failed` 行视为请求失败，而不是一次完成。

## 登录流程

两种变体共用同一套密钥对与解密代码。

1. 生成 2048 位 RSA 密钥对，公钥以 PKCS#1 DER 导出并做 base64url 编码。
2. 返回或打开授权 URL `https://zed.dev/native_app_signin?native_app_port=<port>&native_app_public_key=<key>`。
3. 登录完成后，浏览器被重定向到回调地址，带上 `user_id` 和加密的 `access_token`。
4. 粘贴流程：用户把完整 URL 粘贴到网页或提交给 `POST /zed/login/complete`。服务端用 `zed_login.parseCallbackPayload` 解析查询串，解码 base64url 密文，并用内存中的私钥解密。
5. 本地回调流程：服务端或命令行在回环端口监听，直接从请求行读取同样的参数。
6. `finalizeLogin` 通过 `accounts.addAccount` 把凭据写入 `accounts.json`，重新加载账号池，并把新账号设为当前账号。

同一时间只允许一个登录在进行。粘贴槽与旧槽共用一把互斥锁，并发请求会得到 409。私钥不会离开进程内存，授权有效期为 10 分钟。

## 数据文件

所有文件都在数据目录中（应用 `ZED_DATA_DIR` 之后的工作目录）。

| 文件 | 内容 |
| --- | --- |
| `accounts.json` | 账号映射：名称对应 `user_id`、Zed 凭据 JSON 和可选的 `orb_url`。登录及 `POST /zed/accounts/orb` 写入，启动时读取。 |
| `active_account.txt` | 上次使用的账号名，重启后从它继续，而不是回到第一项。 |
| `api_key` | 可选的网关 key 文件，仅在 `ZED_API_KEY` 与 `ZED_API_KEY_FILE` 都未设置时使用。 |
| `zed-proxy_req_*.json`、`zed-proxy_stream_*.json` | 通过 `--data-binary @file` 交给 `curl` 的短期请求体，请求结束后删除。因此数据目录必须可写，代理才能工作。 |

登录还需要一个可写的临时目录，存放 `openssl` 使用的密文文件（`/tmp`、`/var/tmp` 或 `/dev/shm`，取第一个可写的）。私钥本身通过标准输入传给 `openssl`，不会写入磁盘。
