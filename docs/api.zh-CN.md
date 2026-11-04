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
| GET | `/zed/accounts/status` | 需要 | 不调用模型的令牌与额度状态 |
| POST | `/zed/accounts/health` | 需要 | 单账号或全账号的低成本模型探测 |
| POST | `/zed/accounts/switch` | 需要 | 切换当前账号 |
| GET | `/zed/usage` | 需要 | 当前账号的令牌声明 |
| GET | `/zed/billing` | 需要 | 当前账号的账单与套餐数据 |
| POST | `/zed/login` | 需要 | 发起登录；`{"mode":"paste"}` 选择粘贴流程 |
| POST | `/zed/login/complete` | 需要 | 提交粘贴的回调网址 |
| GET | `/zed/login/status` | 需要 | 轮询登录状态 |
| OPTIONS | 任意路径 | 不需要 | CORS 预检，返回 200 与空响应体 |

其它方法或路径返回 404。
