[← 返回 README](../README.zh-CN.md)

# API 端点

所有路由都在 `src/http/router.rs` 的路由表中处理。「网关 key」表示配置了至少一个 key 时，请求必须通过[鉴权](authentication.zh-CN.md)中描述的检查。`/v1/` 与 `/zed/` 前缀在路由之前校验，因此受保护路径缺 key 或 key 错误得到 401，而不是 404。

| 方法 | 路径 | 网关 key | 作用 |
| --- | --- | --- | --- |
| 任意 | `/` | 不需要 | Web 管理页 |
| GET | `/health` | 不需要 | 存活探针，返回 `{"status":"ok"}` |
| GET | `/v1/models` | 需要 | Zed 的完整模型目录（OpenAI 格式），并附带 Codex 的 `models` 目录。缓存一小时；除 Zed 标记为禁用的模型外不做任何过滤，并额外提供 Sol 的别名 `gpt-5.6`。 |
| 任意 | `/api/event_logging/batch` | 不需要 | 接收客户端事件批次，返回 `{"status":"ok"}` |
| 任意 | `/v1/messages/count_tokens…` | 需要 | 前缀路由（Claude Code 会带后缀）；不做统计，直接返回 `{"input_tokens":0}` |
| POST | `/v1/chat/completions` | 需要 | OpenAI Chat Completions，流式与非流式 |
| POST | `/v1/responses` | 需要 | OpenAI Responses，流式与非流式 |
| POST | `/v1/messages` | 需要 | Anthropic Messages，流式与非流式 |
| GET | `/zed/accounts` | 需要 | 账号列表与调度状态 |
| GET | `/zed/accounts/status` | 需要 | 不调用模型的令牌与额度状态，附带缓存的 Orb 余额 |
| POST | `/zed/accounts/health` | 需要 | 单账号或全账号的低成本模型探测，可指定模型 |
| POST | `/zed/accounts/orb` | 需要 | 设置或清除账号的 Orb portal 链接 |
| POST | `/zed/accounts/orb/refresh` | 需要 | 强制刷新 Orb 余额（0.3.0 新增） |
| POST | `/zed/accounts/switch` | 需要 | 切换当前账号 |
| POST | `/zed/models/refresh` | 需要 | 强制刷新模型目录（0.3.0 新增） |
| GET | `/zed/usage` | 需要 | 当前账号 LLM 令牌的 JWT 载荷（只有声明，不含令牌本身） |
| GET | `/zed/billing` | 需要 | 当前账号从 Zed 取回的 `client/users/me` 原始响应体 |
| POST | `/zed/login` | 需要 | 发起登录；`{"mode":"paste"}` 选择粘贴流程 |
| POST | `/zed/login/complete` | 需要 | 提交粘贴的回调网址 |
| GET | `/zed/login/status` | 需要 | 轮询登录状态 |
| OPTIONS | 任意路径 | 不需要 | CORS 预检；返回 200 与空响应体，下列三条除外 |

`OPTIONS /`、`OPTIONS /api/event_logging/batch` 与 `OPTIONS /v1/messages/count_tokens…` 返回各自正常的非空响应体。未列出的方法（及其它路径）返回 404，不存在 405。

所有响应都带 `Access-Control-Allow-Origin: *`、`Access-Control-Allow-Headers: *`、`Access-Control-Allow-Methods: GET, POST, OPTIONS` 与 `Connection: close`。响应以连接关闭定界，绝不使用 chunked，也不写 `Date`、`Server`、`Transfer-Encoding` 头。`GET /` 额外带 `Cache-Control: no-store`；SSE 响应额外带 `Cache-Control: no-cache`。

`Content-Length` 超过 16 MiB 的请求在读取请求体之前返回 `413 {"error":{"message":"request body exceeds 16 MiB","type":"invalid_request_error"}}`。不支持 chunked 请求体，按空体处理。

## `GET /v1/models`

一个对象里两份列表：

- `data[]`：`{"id","object":"model","owned_by"}`；目录有正数时再追加 `context_length`、`max_output_tokens`、`max_completion_tokens`。三个字段追加在 `owned_by` 之后；缺值、`null` 或非正数时省略该字段，不编造默认值。`max_completion_tokens` 与 `max_output_tokens` 相等。`gpt-5.6` 别名排第一，所有数值复制自 `gpt-5.6-sol`。
- `models[]`：Codex 形态（`slug`、`display_name`、`description` 以 `Zed - ` 开头、模型声明了默认档位时才有 `default_reasoning_level`、`supported_reasoning_levels`、`shell_type`、`visibility`、`supported_in_api`、`priority`）。它不带上面三个数值字段，也不包含被禁用模型。

示例：

```json
{"object":"list","data":[{"id":"gpt-5.6","object":"model","owned_by":"open_ai","context_length":400000,"max_output_tokens":128000,"max_completion_tokens":128000},{"id":"claude-sonnet-5-5","object":"model","owned_by":"anthropic","context_length":1000000,"max_output_tokens":128000,"max_completion_tokens":128000}],"models":[...]}
```

该路由总是返回 200。上游拉取失败时依次退回旧缓存、内置目录；没有任何账号时返回内置目录。

## 代理路由：`POST /v1/chat/completions`、`/v1/responses`、`/v1/messages`

请求体必须是 JSON 对象；其它情况返回 400，消息为客户端协议形状的 `invalid JSON request body`（「不是 JSON」与「是合法 JSON 但不是对象」产生完全相同的字节）。`stream` 为 JSON 布尔 `true` 时走流式；其它值或缺失都按非流式处理。

错误按客户端协议输出：

- OpenAI 路由（Chat 与 Responses）：`{"error":{"message":"…","type":"…","param":null[,"code":"…"]}}`
- Anthropic 路由：`{"type":"error","error":{"type":"…","message":"…"}}`

状态码：

| 情况 | HTTP | 说明 |
| --- | --- | --- |
| 请求体不是 JSON 对象 | 400 | `invalid JSON request body` |
| OpenAI 路由上 GPT-5.6 模型的 `reasoning_effort` 不受支持 | 400 | `code:"unsupported_reasoning_effort"`；`/v1/messages` 豁免 |
| 没有任何账号 | 400 | `no account configured` |
| 上游请求级失败（`400`/`404`/`405`/`413`/`415`/`422`，含 HTTP 200 上的 `status.failed` 包装） | 400 | 保留上游 `message`；不重试、不冷却、不故障转移 |
| 凭据失败（401、非 plan 的 403） | 401 / 403 | 账号冷却；故障转移到下一个账号 |
| plan 级 403 | 403 | 不冷却；故障转移到下一个账号 |
| 限流（429） | 429 | 账号冷却 30 秒 |
| 瞬时失败（5xx、网络、空流） | 502 | 连续 3 次后账号冷却 |

流式响应为 `text/event-stream`，带 `Cache-Control: no-cache` 与 `Connection: close`。上游 HTTP 200 空流每个账号最多重试 2 次后再故障转移。首个模型事件提交之后，后续失败只在流内表达（错误帧后跟协议的结束标记），不再切换账号。

## `POST /zed/accounts/health`

请求体可以是 `{}`（或空）表示全部账号，或 `{"account":"<名称>"}` 表示单个账号，两者都可再加 `"model":"<slug>"`。不带 `account` 时按顺序探测全部账号。

`model` 必须是 `GET /v1/models` 中的 slug 或别名 `gpt-5.6`。未知 slug 返回 `400 {"error":"unknown model"}`；类型不对返回 `400 {"error":"model must be a string"}`。非法 JSON 返回 `400 {"error":"invalid json"}`，非对象体返回 `400 {"error":"request body must be an object"}`。未知账号返回 `404 {"error":"account not found"}`。默认值为 `gpt-5.6-luna`。

探测只发一次低成本请求，并按所选模型从目录派生档位与预算：

| 模型类型 | 发送的 `reasoning_effort` | 报告值 | 报告的输出预算 |
| --- | --- | --- | --- |
| Anthropic provider | 不发 | `null` | `min(32, 目录上限)` |
| GPT-5.5 系列 | 不发（编码器固定 `xhigh`） | `"xhigh"` | `32768` |
| 目录声明了 `none` / `low` | 该值 | 该值 | `32` |
| 没有档位的 OpenAI 模型（如 `gpt-5-nano`） | 不发 | `null` | `1024`（受目录上限约束） |
| Google provider | `"low"`（绝不发 `MINIMAL`） | `"low"` | `min(max(32, 32), 目录上限)`，即 `32` |

响应报告实际使用的模型、推理强度与输出预算，键顺序固定：

```json
{"checked_at":1762300000,"probe":{"model":"gpt-5.6-luna","reasoning_effort":"low","max_output_tokens":32},"accounts":[{"name":"alice","model_ok":true,"model_state":"healthy","model_checked_at":1762300000,"model_latency_ms":640,"scheduler_healthy":true,"cooldown_s":0,"last_status":200}]}
```

`model_state` 取值为 `unchecked`、`healthy`、`auth_error`、`rate_limited`、`upstream_error`；`model_ok` 恒等于 `model_state == "healthy"`。探测失败仍返回 200，并与普通请求一样计入账号冷却。

## `POST /zed/accounts/orb`

保存或清除用于余额显示的 Orb portal 链接：

```json
{"account":"my-account","url":"https://portal.withorb.com/?token=<orb-token>"}
```

URL 必须是 `https`，主机必须恰好是 `portal.withorb.com`（不允许 userinfo，端口只能是默认的 `443`），且必须带非空的 `token` 查询参数，否则返回 `400 {"error":"invalid orb link"}`。`url` 缺失、为 `null` 或空字符串表示清除已保存的链接。未知账号返回 `404 {"error":"account not found"}`，成功返回 `200 {"success":true}`。错误还包括 `400 {"error":"invalid json"}`、`400 {"error":"request body must be an object"}`、`400 {"error":"missing account"}`、`400 {"error":"account must be a string"}`、`400 {"error":"url must be a string"}`。任何响应都不会回显该链接。

## `POST /zed/accounts/orb/refresh`

强制访问门户，绕过成功 5 分钟、失败 60 秒的缓存，每个账号有 5 秒最小间隔：

- 请求：空体、`{}` 或 `{"account":"<名称>"}`。
- `400 {"error":"invalid json"}`、`400 {"error":"request body must be an object"}`、`400 {"error":"account must be a string"}`、`400 {"error":"orb link not configured"}`、`404 {"error":"account not found"}`。
- 5 秒窗口内的刷新不访问门户，直接返回缓存值并附 `retry_after_s`（0–5）。

单账号：

```json
{"refreshed":true,"account":"alice","retry_after_s":0,"credit":{"ok":true,"balance_usd":7.5000,"cap_usd":10.0000,"used_usd":2.5000,"used_pct":25.00,"period_start":"2026-11-01T00:00:00+00:00","resets_at":"2026-12-01T00:00:00+00:00","total_balance_usd":7.5000}}
```

全部账号（账号文件顺序；只含配置了链接的账号）：

```json
{"refreshed":true,"accounts":[{"account":"alice","refreshed":true,"retry_after_s":0,"credit":{...}},{"account":"bob","refreshed":false,"retry_after_s":3,"credit":{...}}]}
```

没有账号，或没有账号配置链接时：`200 {"refreshed":false,"accounts":[]}`。

门户失败仍返回 200，`credit.ok=false`，错误码为 `invalid_link`、`timeout`、`parse_error`、`fetch_failed` 之一；上一次成功的数字仍缓存给 `/zed/accounts/status` 使用。

## `GET /zed/accounts/status`

每个账号条目除令牌、账单、探测与调度字段外还包含：

| 字段 | 含义 |
| --- | --- |
| `orb_configured` | 账号已保存 Orb portal 链接时为 `true` |
| `credit` | 仅在 `orb_configured` 为 true 时出现 |

`credit` 成功时为 `{"ok":true,"balance_usd":7.5,"cap_usd":10.0,"used_usd":2.5,"used_pct":25,"period_start":"2026-11-01T00:00:00+00:00","resets_at":"2026-12-01T00:00:00+00:00","total_balance_usd":7.5}`，获取失败时为 `{"ok":false,"error":"<简短错误码>"}`。`used_pct` 是已用额度占上限的百分比。

`quota_state` 取值为 `available`、`exhausted`、`restricted`、`unmetered`、`unavailable` 或 `unknown`。令牌刷新失败时条目以 `error:"llm_token_refresh_failed"` 结束，后面不再有账单字段。

只对配置了链接的账号拉取余额，逐个进行，每次请求 10 秒超时；结果在内存中按账号缓存 5 分钟，失败的获取缓存 60 秒。因此状态轮询通常返回缓存数字，响应中不会出现链接或 token。

## `POST /zed/accounts/switch`

请求 `{"account":"<名称>"}`。成功返回 `200 {"success":true}`，并把选择持久化到 `active_account.txt`。名字不存在返回 `404 {"error":"not found"}`。请求体非法时返回 `400 {"error":"invalid json"}`、`400 {"error":"request body must be an object"}`、`400 {"error":"missing account"}` 或 `400 {"error":"bad type"}`。切换不改变冷却状态，也不校验目标是否健康。

## `POST /zed/models/refresh`

强制从 Zed 拉取目录，最小间隔 5 秒：

| 情况 | HTTP | 响应体 |
| --- | --- | --- |
| 成功 | 200 | `{"refreshed":true,"count":N}` |
| 5 秒内重复触发 | 200 | `{"refreshed":false,"retry_after_s":s,"count":N}` |
| 无账号 | 200 | `{"refreshed":false,"count":N}`（内置目录） |
| 上游失败（保留旧目录） | 502 | `{"error":"models refresh failed","refreshed":false}` |
| 请求体不是合法 JSON | 400 | `{"error":"invalid json"}` |
| 请求体是合法 JSON 但不是对象 | 400 | `{"error":"request body must be an object"}` |

`count` 是注册表实际收录的 slug 数（不含 `gpt-5.6` 别名与被禁用项）。刷新成功同时重建 `/v1/models` 的缓存。

## `GET /zed/usage`

返回 LLM 令牌的 JWT 载荷，不含签名与完整令牌。载荷就是 base64url 解码后的原始字节：不重新编码、不重排键、不改变空白（令牌的载荷本身是合法 JSON，因此响应体仍是合法 JSON）：

| 情况 | HTTP | 响应体 |
| --- | --- | --- |
| 有账号，令牌已缓存或换取成功 | 200 | 解码后的载荷原始字节，例如 `{"sub":"42","name":"alice","exp":1762300000}` |
| 没有当前账号 | 400 | `{"error":"no account"}` |
| 无法换取令牌或令牌格式错误 | 500 | `{"error":"internal error"}` |

## `GET /zed/billing`

为当前账号请求 `client/users/me`，并把上游响应体原样返回：不重新编码、不重排键、不裁剪字段。响应可能包含账号邮箱与套餐信息，因此受网关 key 保护。

| 情况 | HTTP | 响应体 |
| --- | --- | --- |
| 有账号，上游返回 2xx | 200 | 原始的 `client/users/me` 响应体 |
| 没有当前账号 | 400 | `{"error":"no account"}` |
| 上游非 2xx、传输失败，或响应体超过 1 MiB | 502 | `{"error":"failed to fetch user info"}` |

## 登录路由

`POST /zed/login` 接受可选的 JSON 对象。`{"mode":"paste","name":"work"}` 走粘贴流程，返回 `{"login_url":"https://zed.dev/native_app_signin?native_app_port=58443&native_app_public_key=<b64url>","port":58443,"mode":"paste","expires_in":600}`。其它请求体（含非法体）走本地回调流程，返回 `{"login_url":"…","port":<真实端口>}`。已有登录进行中时再次发起返回 `409 {"error":"login already in progress"}`；`ZED_LOGIN_PORT` 非法返回 `400 {"error":"ZED_LOGIN_PORT must be 1-65535"}`；数据目录不可写返回 500 与 `{"error":"data directory is not writable","detail":"accounts.json cannot be saved; on Linux run chown -R 10001:10001 ./data or set ZED_DATA_DIR"}`。

`POST /zed/login/complete` 接受 `{"url":"<回调网址>"}`（或 `{"query":"…"}`），返回 `{"status":"success","name":"<账号名>"}`。错误包括 `400 {"error":"invalid json"}`、`400 {"error":"missing url"}`、[登录](login.zh-CN.md)中列出的解析与解密错误、`409 {"error":"no pending login"}`、`410 {"error":"pending login expired; start again"}`，以及保存失败（名称过长或凭据不是 JSON 为 400，其它为 `500 {"error":"failed to save account: <短错误名>"}`）。

`GET /zed/login/status` 返回 `{"status":"idle"}`、`{"status":"waiting"}`、`{"status":"success"}` 或 `{"status":"failed"}`；`success` 与 `failed` 读取即消费。该路由不返回账号名、网址或任何密钥。
