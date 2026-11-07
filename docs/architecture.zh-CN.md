[← 返回 README](../README.zh-CN.md)

# 架构

本页梳理模块，以及三条最重要的流程：模型请求、登录与 Orb 余额刷新。端点表和配置表不在此重复，见 [API 端点](api.zh-CN.md)和[配置](configuration.zh-CN.md)。

## 分层

请求沿一个方向进入，响应沿相反方向返回：

```
HTTP 入口（路由表、鉴权关卡、请求体上限）
      ↓
客户端协议解码器（Chat / Responses / Anthropic）→ NeutralRequest
      ↓
provider 编码器（open_ai / anthropic / google）→ provider 请求
      ↓
上游客户端（reqwest、进程内 TLS、代理、超时、体积上限）
      ↓
provider 解码器 → NeutralResponse / StreamEvent
      ↓
客户端协议编码器 → 客户端 JSON / SSE
```

两半之间只共享中立层。客户端解码器不知道将由哪家 provider 执行，provider 也不知道是哪种客户端格式发来的请求。新增第四种客户端格式或第四家 provider 只需新增一个模块，不改动任何既有编码器：三家 provider 之间不互相引用，三份客户端编解码器之间也一样。

provider 分发读取 Zed 目录中该模型的 `provider` 字段。只有目录里没有的名称才回退到 `claude*` → Anthropic、`gpt-*` → OpenAI、`gemini*` → Google 的前缀规则，该规则只有一处实现。不存在硬编码的模型白名单。

## 模块

| 文件 | 职责 |
| --- | --- |
| `src/main.rs` | 入口。解析子命令（`serve`、`login`、`accounts`、`healthcheck`），打印用法并转交。 |
| `src/config.rs` | 环境解析与校验：监听地址与端口、登录端口、数据目录、上游基址、出站代理。纯函数实现，便于在不改进程环境的前提下测试规则。 |
| `src/http/server.rs` | 监听与连接循环。读取一个请求头及其 `Content-Length` 请求体，经路由分发，缓冲响应写精确 `Content-Length`，流式响应以连接关闭定界。不使用 `axum::serve`，因为 hyper 对未知长度响应必然 chunked 且必然加 `Date`。 |
| `src/http/router.rs` | 路由表与各 handler，包括入口守卫（鉴权 → 16 MiB 上限 → `OPTIONS` 处理 → 公共 CORS/Connection 头）。 |
| `src/http/auth.rs` | 网关 key 加载（`ZED_API_KEY`、`ZED_API_KEY_FILE`、`<data>/api_key`）、SHA-256 常量时间比较、`Authorization`/`x-api-key` 解析、受保护前缀、回环判断、fail-closed 绑定决策与拒绝文案。 |
| `src/http/body.rs` | 请求体上限、JSON 对象闸门、精确的 413/404 错误体与响应构造。 |
| `src/http/static.rs` | 内嵌网关面板（`include_bytes!` 读 `webui/dist/index.html`），由 `GET /` 返回并带 `Cache-Control: no-store`。测试检查产物是完整 HTML 文档且包含五个页面锚点。 |
| `src/client/chat.rs`、`responses.rs`、`anthropic.rs` | 每种客户端协议一个解码器和一个编码器：请求 JSON → `NeutralRequest`，`NeutralResponse` / `StreamEvent` → 客户端 JSON 或 SSE。键顺序是规范的一部分，因此 JSON 手工写出。 |
| `src/neutral/` | 共享数据类型：请求、消息与内容块、工具与工具选择、响应、流事件、结束原因与用量。 |
| `src/provider/openai.rs`、`anthropic.rs`、`google.rs` | 各 provider 的中立请求编码与上游事件解码。各自负责推理档位、输出预算、采样规则、工具方言与事件形状。`openai.rs` 面向 Responses API，`anthropic.rs` 面向 Messages API，`google.rs` 面向 `generateContent`。 |
| `src/provider/google_schema.rs` | 把客户端 JSON Schema 清洗为 Gemini 子集（允许键、`nullable` 处理、不支持的结构），是该方言唯一的实现处。 |
| `src/provider/mod.rs` | 工厂（`encoder_for`）与极小的公共信封辅助函数。 |
| `src/upstream/client.rs` | 唯一访问网络的模块：`reqwest` + rustls、四个端点、超时（非流式 120 秒、流式 300 秒、元数据 15 秒）、体积上限（4 MiB / 1 MiB）、`HTTPS_PROXY`/`ALL_PROXY` 代理、`Zed/1.8.2` user agent，以及 Zed 客户端头常量。同时负责 `status.failed` 包装与 HTTP 状态的分类。 |
| `src/upstream/token.rs` | 每账号 LLM 令牌缓存与 `client/llm_tokens` 换取，提前 60 秒刷新；从 `client/users/me` 选择 `organization_id`；解析 `ZED_SYSTEM_ID`（非空时才发送）。 |
| `src/upstream/models.rs`、`jwt.rs` | `/models` 拉取与目录重建；JWT 载荷与 `exp` 读取（不验签）用于缓存过期与 `/zed/usage` 的原文响应体。 |
| `src/catalog.rs` | slug → 能力注册表。由 Zed 的 `/models` 响应或内嵌 `src/models.json` 构建；生成 `/v1/models` 载荷（OpenAI `data[]` 加 Codex `models[]`）；包含 `gpt-5.6` 别名、唯一的模型名归一化表，以及一小时列表缓存与 5 秒刷新窗口。 |
| `src/accounts/store.rs` | 账号池：宽容加载 `accounts.json`、原子 0600 写入（临时文件 + rename）、当前账号持久化、冷却/尝试状态、尝试顺序与成功失败记账。 |
| `src/accounts/status.rs` | `/zed/accounts` 的调度行、`/zed/accounts/status` 的被动令牌/账单检查，以及两份固定形状的响应体。 |
| `src/proxy/mod.rs` | 请求编排：解码、校验档位、选 provider、按冷却遍历账号、一次 401 令牌重铸、错误分类、非流式尝试循环与流式预读/提交外壳。 |
| `src/proxy/stream.rs` | SSE 帧状态、带 64 KiB 上限的行缓冲、空流重试常量与客户端可见的流类型。 |
| `src/probe.rs` | 实测请求：固定请求体、目录驱动的档位与预算、单次不转移的尝试、成功判定，以及把探测结果写回账号。 |
| `src/orb.rs` | Orb 链接校验、token 提取与再编码、带浏览器头的门户请求、ledger 解析与聚合、固定 credit JSON、300 秒/60 秒缓存与强制刷新规则。 |
| `src/login/mod.rs` | 进程内 RSA 2048 密钥对、PKCS#1 DER 公钥、授权 URL、回调解析、OAEP-SHA256 解密、带代数号的单登录槽、粘贴与本地回调流程、命令行登录。 |
| `src/error.rs` | 错误分类（`request`、`auth`、`plan`、`rate_limit`、`transient`、`client_disconnect`）、HTTP 状态与各协议错误体。 |
| `webui/` | 网关面板源码。`index.html` 是静态骨架（侧栏、顶栏、五个页面容器、对话框、无脚本兜底）；`src/main.ts` 负责启动顺序、导航与逐页渲染隔离；`src/text.ts` 保存全部中英可见文案；`src/state.ts`、`api.ts`、`data.ts`、`store.ts`、`format.ts` 分别负责语言与存储、请求与网关密钥、容错 JSON 解析与目录缓存、共享视图状态、格式化；`ui.ts`、`dom.ts`、`dialog.ts` 是共享块；`src/pages/*.ts` 每页一个渲染器；`src/selfcheck.ts` 是仅供开发、不进产物的自检脚本；`src/theme.css` 是全部样式与设计令牌。Vite 加 `vite-plugin-singlefile` 把脚本与样式内联，产物是单个不含外部域名的 `dist/index.html`，字体全部来自系统栈。 |
| `build.rs` | 在 crate 编译前运行：只把 `webui/dist/index.html` 登记为 `cargo:rerun-if-changed` 输入，并在该文件缺失时以明确的「先构建 WebUI」信息让构建失败，避免二进制静默携带空白页面。产物之外没有资源目录，运行时也不读任何文件。 |
| `tools/genmodels.py`、`tools/catalog-fixture.json` | 重新生成内嵌兜底目录：脚本读取脱敏的 Zed `/models` 夹具并写出 `src/models.json`（键顺序是规范；C3 的三个上限字段仅在为正整数时复制）。运行 `python3 -I tools/genmodels.py`；脚本只用标准库，两个路径都相对脚本自身位置解析。 |

## 请求流程

1. `serve_connection` 读取请求行与请求头，并读取最多 16 MiB 的请求体（`Content-Length` 更大时仅凭请求头即拒绝）。不支持 chunked 请求体。
2. 配置了至少一个 key 时，入口守卫先校验 `/v1/` 与 `/zed/` 请求，再应用 16 MiB 上限、应答 `OPTIONS`，并加上公共头。
3. 路由按方法与路径分发。代理路由先把请求体解析为 JSON 对象，再按 `stream` 分支。两条 OpenAI 路由上的 GPT-5.6 模型会在任何上游调用之前校验 `reasoning_effort`；`/v1/messages` 豁免。
4. `proxy::prepare` 把客户端请求体解码为中立请求，经目录归一化模型名，按目录解析 provider 并选出编码器。
5. 非流式：`handle_non_stream` 根据账号池生成尝试顺序，对每个账号获取或复用 LLM 令牌、编码 provider 请求、POST 到 `completions` 并解码响应。遇到值得 failover 的错误时给账号记账并改试下一个；请求级 400 类失败立即结束请求。
6. 流式：`handle_stream` 使用同样的尝试顺序，但会预读上游直到首个模型事件，使客户端看到任何字节之前的失败仍可重试（空 200 流最多 2 次）或转移。提交点之后响应头已发出，后续失败只能作为流内错误帧表达。
7. HTTP 200 上的 `status.failed` 对象视为请求失败，而不是一次完成。`stream_ended` 行正常结束上游流。
8. 失败被分类为请求、凭据、plan、限流或瞬时，据此决定冷却、重试与转移（未对真实上游验证的部分见[已知限制](limitations.zh-CN.md)）。

## 登录流程

两种变体都生成新的密钥对，并使用同一套解析与解密代码。

1. 进程内生成 RSA 2048 位密钥对（公开指数 65537）。公钥以 PKCS#1 `RSAPublicKey` DER 导出，并做无填充 base64url 编码。
2. 授权 URL `https://zed.dev/native_app_signin?native_app_port=<port>&native_app_public_key=<key>` 在粘贴流程中直接返回，在本地流程中返回并打印。不会打开浏览器；URL 始终出现在响应或日志里。
3. 登录完成后，浏览器跳到 `http://127.0.0.1:<port>/?user_id=…&access_token=…`。
4. 粘贴流程：用户把完整 URL 粘贴到页面或提交给 `POST /zed/login/complete`。服务端丢弃 fragment，解析查询串（`+` 与 `%XX` 解码、重复键后者生效、先查 `user_id` 再查 `access_token`），并用内存中的私钥以 RSA-OAEP-SHA256 解密 base64url 密文。
5. 本地回调流程：服务端或命令行在回环端口监听，从请求行读取同样的参数，并返回 `302` 到 `https://zed.dev/native_app_signin_succeeded`。
6. 凭据经 `AccountStore::add_account` 写入 `accounts.json`（临时文件 + rename 原子替换），重载账号池，并把新账号设为当前账号。

同一时间只允许一个登录在进行；第二次发起得到 409。私钥不会离开进程内存，不会写入文件，也不会传给子进程；授权有效期为 10 分钟。不存在 `openssl` 命令行调用与临时密文文件。

## Orb 刷新流程

`POST /zed/accounts/orb/refresh` 选出目标账号（指定账号，或全部配置了链接的账号），对每个账号：

1. 检查距上次真正访问门户是否已过 5 秒；窗口内直接用缓存应答并给出 `retry_after_s`。
2. 依次请求 `api/v1/customer_from_link` 与 `api/v1/customers/<id>/ledger_summary`，都带浏览器仿真头、10 秒超时与 1 MiB 上限。
3. 把 credit 块聚合为固定 credit 对象并缓存，成功 300 秒、失败 60 秒。失败时保留上一次成功的数字供 `/zed/accounts/status` 使用，而刷新响应本身报告 `credit.ok=false`。

所有请求都走进程内客户端，代理设置取自环境；token 与链接不会出现在响应、日志或进程参数列表中。

## Web UI 的嵌入方式

网关面板不是一个独立服务，运行时也没有磁盘上的资源：

1. `cd webui && npm ci && npx tsc --noEmit && npm run build` 先做类型检查，再写出单个 `webui/dist/index.html`，JavaScript 与 CSS 已内联。没有 `dist/assets/`，页面不引用任何外部域名，字体来自系统栈。
2. `build.rs` 在 crate 编译前运行：把 `webui/dist/index.html` 登记为重建输入，文件缺失时直接 panic 并给出明确提示，所以不可能产出一个没有页面的二进制。
3. `src/http/static.rs` 用 `include_bytes!` 在编译期嵌入该文件，由 `GET /` 以 `text/html; charset=utf-8` 返回，并带 `Cache-Control: no-store`。它的测试断言字节是完整 HTML 文档且包含五个页面容器（`page-accounts`、`page-models`、`page-health`、`page-endpoints`、`page-integration`），构建不完整的产物会让 `cargo test` 失败。
4. 字节在编译期固定，改动面板必须依次重建产物与二进制；运行中的进程从不读取 `webui/`，也不需要它的写权限。页面只把三个键保存在浏览器一侧：`zed_lang`、`zed_api_key`、`zed_probe_model`。

## 数据文件

所有文件都在数据目录中（应用 `ZED_DATA_DIR` 之后的工作目录）。

| 文件 | 内容 |
| --- | --- |
| `accounts.json` | 账号映射：名称对应 `user_id`、Zed 凭据 JSON 和可选的 `orb_url`。登录及 `POST /zed/accounts/orb` 写入，启动时读取；通过 0600 临时文件加 rename 原子写入。 |
| `active_account.txt` | 上次使用的账号名，重启后从它继续，而不是回到第一项。 |
| `api_key` | 可选的网关 key 文件，仅在 `ZED_API_KEY` 与 `ZED_API_KEY_FILE` 都未设置时使用。 |
| `.write_probe` | 登录前以 `0600` 创建后立即删除，用于确认目录可写。 |
| `.accounts.json.tmp`、`.active_account.txt.tmp` | 原子写入使用的同目录临时文件，rename 后即消失。 |

0.2.2 的 `zed-proxy_req_*.json`、`zed-proxy_stream_*.json` 与 `zed-proxy_enc_<pid>_<n>.bin` 已不存在：请求体与密文都不再落盘。
