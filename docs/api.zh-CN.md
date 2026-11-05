[← 返回 README](../README.zh-CN.md)

# API 端点

所有路由都在 `src/server.zig` 中处理。「网关 key」表示配置了至少一个 key 时，请求必须通过[鉴权](authentication.zh-CN.md)中描述的检查。

| 方法 | 路径 | 网关 key | 作用 |
| --- | --- | --- | --- |
| POST | `/v1/responses` | 需要 | OpenAI Responses，流式与非流式；不含 Gemini |
| POST | `/v1/chat/completions` | 需要 | OpenAI Chat Completions，流式与非流式 |
| POST | `/v1/messages` | 需要 | Anthropic Messages，流式与非流式 |
| 任意 | `/v1/messages/count_tokens` | 需要 | 不做统计，直接返回 `{"input_tokens":0}` |
| 任意 | `/api/event_logging/batch` | 不需要 | 接收客户端事件批次，返回 `{"status":"ok"}` |
| GET | `/v1/models` | 需要 | OpenAI 格式的模型列表 |
| GET | `/health` | 不需要 | 存活探针，返回 `{"status":"ok"}` |
| 任意 | `/` | 不需要 | Web 管理页 |
| GET | `/zed/accounts` | 需要 | 账号列表与调度状态 |
| GET | `/zed/accounts/status` | 需要 | 不调用模型的令牌与额度状态，附带缓存的 Orb 余额 |
| POST | `/zed/accounts/health` | 需要 | 单账号或全账号的低成本模型探测，可指定模型 |
| POST | `/zed/accounts/orb` | 需要 | 设置或清除账号的 Orb portal 链接 |
| POST | `/zed/accounts/switch` | 需要 | 切换当前账号 |
| GET | `/zed/usage` | 需要 | 当前账号的令牌声明 |
| GET | `/zed/billing` | 需要 | 当前账号的账单与套餐数据 |
| POST | `/zed/login` | 需要 | 发起登录；`{"mode":"paste"}` 选择粘贴流程 |
| POST | `/zed/login/complete` | 需要 | 提交粘贴的回调网址 |
| GET | `/zed/login/status` | 需要 | 轮询登录状态 |
| OPTIONS | 任意路径 | 不需要 | CORS 预检，返回 200 与空响应体 |

其它方法或路径返回 404。

## `POST /zed/accounts/health`

请求体可以是 `{}`（全部账号）或 `{"account":"<名称>"}`（单个账号），两者都可再加 `"model":"<slug>"`。不带 `account` 时按顺序探测全部账号。

`model` 必须是 `GET /v1/models` 中的 slug。未知的 slug 返回 `400 {"error":"unknown model"}`。默认值为 `gpt-5.6-luna`。探测只发一次低成本请求，并按所选模型构造：GPT-5.6 模型带 `reasoning_effort: "none"`，Claude 模型不带该字段，Gemini 模型带 `"low"` 且绝不发送 `MINIMAL`，GPT-5.5 不带该字段因为它被转换层固定为 `xhigh`。

响应中的 probe 对象反映实际使用的模型、推理强度与输出预算，三者都会随选择变化。Gemini 的 `max_output_tokens` 为 `null`，因为它的请求不带输出上限；GPT-5.5 系列固定为 `xhigh`，转换层会把预算抬到大推理下限，因此报告 `32768`。使用默认值时：

```json
{"checked_at":1762300000,"probe":{"model":"gpt-5.6-luna","reasoning_effort":"none","max_output_tokens":32},"accounts":[...]}
```

## `POST /zed/accounts/orb`

保存或清除用于余额显示的 Orb portal 链接：

```json
{"account":"my-account","url":"https://portal.withorb.com/?token=<orb-token>"}
```

URL 必须是 `https`，主机必须恰好是 `portal.withorb.com`（不允许 userinfo，端口只能是默认的 `443`），且必须带非空的 `token` 查询参数，否则返回 `400 {"error":"invalid orb link"}`。`url` 为空字符串表示清除已保存的链接。未知账号返回 404，成功返回 `200 {"success":true}`。任何响应都不会回显该链接。

## `GET /zed/accounts/status`

每个账号条目除令牌与额度字段外还包含：

| 字段 | 含义 |
| --- | --- |
| `orb_configured` | 账号已保存 Orb portal 链接时为 `true` |
| `credit` | 仅在 `orb_configured` 为 true 时出现 |

`credit` 成功时为 `{"ok":true,"balance_usd":7.5,"cap_usd":10.0,"used_usd":2.5,"used_pct":25,"period_start":"2026-11-04T00:00:00+00:00","resets_at":"2026-12-04T00:00:00+00:00","total_balance_usd":7.5}`，获取失败时为 `{"ok":false,"error":"<简短错误码>"}`，错误码取值为 `invalid_link`、`timeout`、`parse_error` 或 `fetch_failed`。`used_pct` 是已用额度占上限的百分比。

只对配置了链接的账号拉取余额，逐个进行，每次请求 10 秒超时；结果在内存中按账号缓存 5 分钟，失败的获取缓存 60 秒。因此状态轮询通常返回缓存数字，响应中不会出现链接或 token。
