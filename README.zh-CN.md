[English](README.md) | 简体中文

# ZedProxy

[![CI](https://github.com/wray-lee/zed-proxy/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/wray-lee/zed-proxy/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)

ZedProxy 是一个 HTTP 网关，用 OpenAI Chat Completions、OpenAI Responses 和 Anthropic Messages 三种请求格式访问 Zed 托管的模型（包括 `gemini-4-argon`）。

Zed 的模型服务只接受它自己的请求结构，并要求一个已登录的 Zed 账号。ZedProxy 接受各编程客户端已经在用的 API 形状，把请求转换成 Zed 的格式，用账号池里的凭据转发到 `cloud.zed.dev`，再把响应转换回来。这样 OmniRoute、Codex、Claude Code 等客户端就能通过各自习惯的 OpenAI 或 Anthropic 配置使用 Zed 托管的模型。

## 功能

- OpenAI Chat Completions（`POST /v1/chat/completions`），支持流式与非流式。
- OpenAI Responses（`POST /v1/responses`），支持流式与非流式。该链路不承载 Gemini 模型。
- Anthropic Messages（`POST /v1/messages`），支持流式与非流式。
- Gemini 思考支持：客户端 `reasoning_effort` 映射到 Gemini `thinkingLevel`，上游 `thought: true` 的 part 拆到思考输出，不混进正文。
- 账号池、冷却与 failover。鉴权失败、限流、连续上游错误会让账号进入冷却，下一次请求改试其它账号。
- `/` 提供 Web 管理页。
- 网关 API key 鉴权，绑定策略 fail-closed：非回环地址且未配置任何 key 时拒绝启动。
- Docker 镜像与 compose 示例。
- 容器/远程部署可用的粘贴回调登录。
- `/health` 存活探针。

## 快速开始

```sh
cp docker-compose.example.yml docker-compose.yml
cp .env.example .env          # 然后把 ZED_API_KEY 改成自己的值
docker compose up -d --build
curl -s http://127.0.0.1:8001/health
```

compose 文件从当前目录构建，镜像固定命名为 `zed-proxy:local`。只发布宿主机 `127.0.0.1:8001`，数据落在 `./data`。若镜像已存在，删掉 `build: .` 一行，保留 `image: zed-proxy:local`。

Linux 上让容器用户（uid 10001）可写数据目录：

```sh
chown -R 10001:10001 ./data
chmod 700 ./data
```

## 鉴权

网关 key 按以下顺序读取，第一个产生非空值的数据源生效：

1. `ZED_API_KEY` 环境变量
2. `ZED_API_KEY_FILE` 指向的文件
3. `<数据目录>/api_key`（容器内为 `/data/api_key`）

多个 key 用逗号或换行分隔；首尾空白和空项会被丢弃。可以一次配置多个 key。文件读不出来时视为没有 key，不会再回退到下一个数据源。

受保护路径是 `/v1/` 与 `/zed/` 开头的前缀。访问受保护路径必须带以下任一头部：

```
Authorization: Bearer <key>
x-api-key: <key>
```

`/health` 与 `/` 免鉴权。`/api/event_logging/batch` 也免鉴权，因为它不在上述两个前缀内；它只确认收到客户端事件批次，不保存任何内容。浏览器的 `OPTIONS` 预检同样不需要 key。

完全没有配置 key 且监听回环地址时，鉴权关闭，与以前的本机用法一致。监听地址不是回环且没有任何 key 时，进程打印拒绝原因并以状态码 1 退出，不会把无鉴权的代理暴露出去。

## 登录

登录会在数据目录写入或更新 `accounts.json`。Zed 的登录流程会把浏览器重定向回发起登录那台机器上的回调端口。Docker 里该端口从浏览器不可达，因此使用粘贴流程。

### Docker：粘贴回调

1. 打开 `http://127.0.0.1:8001/`，进入账号页。
2. 点击添加 Zed 账号。
3. 点击授权链接，在 Zed 页面完成登录。
4. 浏览器会跳到 `http://127.0.0.1:<ZED_LOGIN_PORT>/...`。容器内这个页面打不开是预期现象。
5. 复制地址栏里的完整网址，粘贴到账号页输入框并提交。账号列表刷新并出现新账号即成功。

`ZED_LOGIN_PORT` 默认 `58443`，只用于拼接授权链接里的回调地址，这个模式下没有进程需要监听它。授权链接 10 分钟内有效，RSA 私钥只存在进程内存中，一次授权只对应一次登录，同一时间只允许一个登录。

### 宿主机：命令行

```sh
zig build
./zig-out/bin/zed-proxy login my-account
```

该命令在默认浏览器里打开授权链接，在随机回环端口监听回调，回调到达后把账号保存为 `my-account`。等待上限为 10 分钟。

### 服务端本地回调模式

`POST /zed/login` 不带 `mode` 字段时，在运行中的服务里走同一套本地回调流程：监听随机的 `127.0.0.1` 端口、打开浏览器、最多等待 10 分钟。Web 页面只用粘贴流程，所以这种模式只在回调端口可达时有用，例如服务直接跑在宿主机上。

## 客户端配置

以下都是示例配置，不是硬性要求。任何会发这三种请求格式之一的客户端都可以使用对应的链路。

### OmniRoute（OpenAI 兼容上游）

- Base URL：`http://<宿主机地址>:8001/v1`
- API key：与 `ZED_API_KEY` 相同的值
- Model：`gemini-4-argon`

Chat Completions 请求用 `reasoning_effort`（或 `reasoning.effort`）传思考档位：

| 客户端传入 | Gemini `thinkingLevel` |
| --- | --- |
| 不传 / `none` | 不设 level（`includeThoughts` 仍为 true） |
| `minimal` | `LOW`（`gemini-4-argon` 和 `gemini-3.8` 会拒绝 `MINIMAL`） |
| `low` | `LOW` |
| `medium` | `MEDIUM` |
| `high` / `xhigh` / `max` / 未知值 | `HIGH` |

思考内容以 `thought: true` 的 part 返回。非流式响应在 Chat Completions 上放到 `message.thinking` 与 `message.reasoning_content`，在 Anthropic Messages 上放到 `thinking` 块；流式响应在 Chat 上放到 `reasoning_content`，在 Anthropic 上放到 thinking 块。正文在两种情况下都与思考分开。

### Claude Code

```sh
export ANTHROPIC_BASE_URL=http://127.0.0.1:8001
export ANTHROPIC_AUTH_TOKEN=<与 ZED_API_KEY 相同的值>
export ANTHROPIC_MODEL=claude-sonnet-5
```

`configs/claude.settings.json.example` 是同样设置的 JSON 形式。`ANTHROPIC_AUTH_TOKEN` 只有在配置了网关 key 时才会被校验；仅监听回环且没有 key 的实例可以填任意非空字符串。

## 端点

所有路由都在 `src/server.zig` 中处理。「网关 key」表示配置了至少一个 key 时，请求必须通过上面的鉴权。

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

## 配置

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

## 架构

```
客户端 -> 网关鉴权 -> 路由 -> providers（请求转换）
       -> 账号池 -> cloud.zed.dev/completions
       -> providers（响应与流转换）-> 客户端
```

- `src/server.zig`：HTTP 服务、路由表、鉴权入口、账号端点、登录端点、模型列表。
- `src/gateway_auth.zig`：加载 key、解析 `Authorization` 与 `x-api-key`、执行绑定策略。
- `src/providers.zig`：把各客户端协议转换成 Zed 请求体，再把上游响应转换回来；Gemini 思考映射也在这里。
- `src/accounts.zig`：账号池、冷却、尝试顺序、持久化的当前账号。
- `src/stream.zig`：把上游流式事件转换成各协议的 SSE。
- `src/proxy.zig` 与 `src/zed.zig`：配置了代理时用 `curl` 请求 `cloud.zed.dev`，否则用 Zig 的 HTTP 客户端。
- `src/auth.zig` 与 `src/zed_login.zig`：RSA 密钥对与两种登录流程。
- `webui/`：TypeScript 页面，构建成单个 HTML 文件，编译期嵌入二进制。
- 流式链路、以及配置了代理时的所有上游调用都需要 `curl`；Linux 上的登录与解密需要 `openssl`。Docker 镜像里两者都已安装。

模块表、请求流、登录流与数据文件的细节见 [docs/architecture.md](docs/architecture.md)。

## 已知限制

- 粘贴回调只实现 RSA-OAEP-SHA256 解密。OmniRoute 的 Zed 辅助代码在 OAEP-SHA256 失败后还会回退 RSA PKCS#1 v1.5。真实 Zed 回调使用哪种 padding 未经真实账号验证。
- Web 页面只提供粘贴流程。本地回调监听仍然存在，可由 `POST /zed/login` 或命令行触发。
- 真实 Zed 登录回调与真实 `gemini-4-argon` 调用未经真实账号验证，此前所有测试与探测都使用构造凭据或本地假数据。
- arm64 镜像只在 QEMU 下构建并启动，未在真实 arm64 硬件上运行。
- Windows BCrypt 登录分支只通过编译，未实际运行。
- `/v1/responses` 不承载 Gemini，只有 Chat Completions 与 Anthropic Messages 会转发到 Google。
- 单用户设计，没有按客户端的配额、限流或多租户隔离。
- 上游请求模仿 Zed 客户端，上游协议随时可能变化，不保证长期可用。
- `ZED_VERSION` 在编译期固定为 `1.8.2`。上游接受的客户端版本变化时，模型或套餐判定可能随之改变。
- 除非设置 `ZED_SYSTEM_ID`，换取令牌时不发送 system id。作者的检查中，普通账号不带它也能换取令牌并完成请求；如果 Zed 对试用账号返回 `trial_blocked`，请把 `ZED_SYSTEM_ID` 设为你的 Zed 客户端注册时使用的 id。
- 上游凭据通过 `-H` 参数传给 `curl`，会出现在进程参数列表中，同机其他用户可读。见 [SECURITY.md](SECURITY.md)。

## 非官方声明与条款风险

ZedProxy 是非官方项目，与 Zed Industries, Inc. 没有关联，也未获其背书或支持。「Zed」只用于指代本代理访问的上游服务。

配置账号后，代理会以你的名义向 Zed 运营的端点发送请求：

- `cloud.zed.dev/completions` 发送 `authorization`、`content-type`、`x-zed-version: 1.8.2`、`x-zed-client-supports-status-messages: true`、`x-zed-client-supports-stream-ended-request-completion-status: true`、`user-agent: Zed/1.8.2 (<os>; <arch>)`。
- `cloud.zed.dev/client/llm_tokens` 发送 `authorization`、`content-type`、`x-zed-system-id`（仅在设置 `ZED_SYSTEM_ID` 时）和相同的 `user-agent`。
- `cloud.zed.dev/models` 发送 `authorization`、`x-zed-version` 和相同的 `user-agent`。
- `cloud.zed.dev/client/users/me` 发送 `authorization`、`accept`、`content-type` 和相同的 `user-agent`。
- 登录会打开 `zed.dev/native_app_signin`（带生成的公钥与回调端口）。本地回调模式下，回调解析完成后浏览器被重定向到 `zed.dev/native_app_signin_succeeded`；粘贴模式则把回调网址粘回给代理。

这些头部与标识的作用是让流量看起来来自官方客户端。账号的套餐、试用状态和风控判定都基于它们。

Zed 的服务条款（`legal/terms.md`，最后更新于 2026-03-02，发布于 https://zed.dev/terms ）中有与这类用法相关的条款。下面列出出处，便于逐条阅读原文；这是引用清单，不是对条款的法律解释。

- §2.2（Access Grant）：Service 只能用于内部业务目的，或个人非商业目的，并须遵守 Zed 的文档；客户同意只通过 Zed 指定的机制访问 Service。
- §2.4(a)：客户不得向第三方提供、分发、出售或再许可 Service 的访问权。
- §2.4(b)：客户不得试图访问与 Service 关联的非公开 API。
- §2.4(d)：客户不得干扰 Service 的运作、绕过访问限制，或对 Service 进行安全或漏洞测试。
- §6（Suspension of Service）：Zed 可立即暂停访问，包括违反 §2.2 至 §2.4 或 §5 的情形。
- §8（Disclaimers; No Warranties by Zed）：软件、服务与输出按「as is」「as available」提供，不作任何保证。
- §9.2（Termination）：客户违反任一规定即无权访问 Service，条款自动终止；Zed 也可在任何时候以任何理由、有无通知地暂停或终止访问。
- §12（Indemnity）：客户须就未经授权使用或滥用 Service、客户数据、输出使用、违反条款等引起的第三方索赔为 Zed 抗辩并赔偿。

§2.3 还引入了 https://zed.dev/acceptable-use-policies 列出的可接受使用政策，包括 Anthropic Usage Policy、Google Generative AI Prohibited Use Policy、OpenAI Usage Policies、GitHub Acceptable Use Policies（Zed 仓库的 `legal/third-party-terms.md` 列出同一组政策）。

实际后果是：本项目可能以 Zed 未预期的方式转发你自己账号的流量，账号可能被暂停或终止，使用风险由你承担。只用自己的账号，不要分享网关访问权，不要把它暴露到公网。本节不构成法律建议。

## 致谢

- [csmsqj/zed-api](https://github.com/csmsqj/zed-api) 是本项目的基础。该项目为 MIT 许可，其版权声明保留在 [LICENSE](LICENSE) 中。
- [yukmakoto/zed2api](https://github.com/yukmakoto/zed2api) 的 PR #2 仅作 Gemini 思考参数映射思路的参考，没有复制代码；该仓库未声明 license。
- TraeWorkAssistant 仅作管理界面思路参考，没有复制代码。

## 许可证

MIT，见 [LICENSE](LICENSE)。原始版权归 csmsqj；修改部分版权 2026 wray-lee。

## 开发

贡献规则见 [CONTRIBUTING.md](CONTRIBUTING.md)。

```sh
zig build          # 生成 zig-out/bin/zed-proxy
zig build test     # 运行单元测试
docker build -t zed-proxy:local .
```

需要 Zig 0.15.2 或更新版本。`zig build` 之前必须先在 `webui/` 里执行一次 `npm ci && npm run build`，因为二进制会嵌入 `webui/dist/index.html`。
