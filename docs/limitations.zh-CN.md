[← 返回 README](../README.zh-CN.md)

# 已知限制

- 粘贴回调只实现 RSA-OAEP-SHA256 解密。OmniRoute 的 Zed 辅助代码在 OAEP-SHA256 失败后还会回退 RSA PKCS#1 v1.5。真实 Zed 回调使用哪种 padding 未经真实账号验证。
- Web 页面只提供粘贴流程。本地回调监听仍然存在，可由 `POST /zed/login` 或命令行触发。
- 不会自动打开浏览器。`zed-proxy login` 与 `POST /zed/login` 只打印或返回授权网址，需要用户自己打开；0.2.2 会尝试 `xdg-open` 等命令。
- 真实 Zed 登录回调与真实 `gemini-4-argon` 调用未经真实账号验证，此前所有测试与探测都使用构造凭据或本地假数据。
- arm64 镜像只在 QEMU 下构建并启动，未在真实 arm64 硬件上运行。
- `ZED_UPSTREAM_BASE` 用于把代理指向假上游做差分验证。它只对本地假上游用过；真实 `cloud.zed.dev` 配自定义前缀时的行为未经验证。
- Rust 重写的真实上游验证仍待完成。0.3.0 的行为是对本地假上游与 0.2.2 差分基线验证的，未用真实账号对 `cloud.zed.dev` 验证。
- Gemini 工具调用未经真实上游验证：schema 清洗、工具调用往返、输出上限（现在会按目录上限下发 `generationConfig.maxOutputTokens`，尚无真实调用确认上游接受该值）、`inlineData` 图片、`toolConfig`/`functionCallingConfig` 以及 `thoughtSignature` 透传都属于这一类。
- OpenAI provider 的输出预算按目录上限截断（例如 999999 会变成 128000）。只有本地假上游接受或拒绝过这一行为；Zed 真实上游是否同样截断未经验证。
- Gemini 的 `systemInstruction` 映射与 `thinkingLevel` 映射只有本地测试覆盖。
- 发往 Gemini 模型的 `/v1/responses` 已实现，但转换矩阵中每个相关格子都只对假上游验证过。
- 单用户设计，没有按客户端的配额、限流或多租户隔离。
- 上游请求模仿 Zed 客户端（`user-agent: Zed/1.8.2 (linux; x86_64)`），上游协议随时可能变化，不保证长期可用。
- `ZED_VERSION` 在编译期固定为 `1.8.2`。上游接受的客户端版本变化时，模型或套餐判定可能随之改变。
- 除非设置 `ZED_SYSTEM_ID`，换取令牌时不发送 system id。作者的检查中，普通账号不带它也能换取令牌并完成请求；如果 Zed 对试用账号返回 `trial_blocked`，请把 `ZED_SYSTEM_ID` 设为你的 Zed 客户端注册时使用的 id。
- `GET /zed/usage` 与 `GET /zed/billing` 是面向真实账号的管理端点：前者换取（或复用）LLM 令牌后只返回其 JWT 载荷，逐字节取令牌中的原文，后者原样代理 `client/users/me`，上游任何失败都返回 502。没有当前账号时两者都返回 400 `{"error":"no account"}`。它们只在本地假上游上验证过；Web UI 不调用它们。
- Orb 余额来自非官方 portal API（`portal.withorb.com/api/v1`），不来自 Zed，也没有公开契约。它可能随时变化或失效，届时解析会返回错误码而不是余额。余额按账号缓存 5 分钟（失败后 60 秒），强制刷新每个账号最多 5 秒一次，因此可能滞后门户。
- Orb 链接是放在 URL 查询串里的 bearer token，拿到它的人可以读取对应客户的 Orb 数据，请当作密码对待。它以明文存放在 `accounts.json`，API 不会返回它，日志也不会写入它。
- 流式请求在首个模型事件提交时就记为成功；此后的失败（或客户端断开）不会记到账号头上。
- `accounts.json` 上限 4 MiB，账号名与 `user_id` 上限 256 字节；超出上限的登录会被拒绝。
