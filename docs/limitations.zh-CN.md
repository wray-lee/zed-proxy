[← 返回 README](../README.zh-CN.md)

# 已知限制

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
- 上游凭据通过 `-H` 参数传给 `curl`，会出现在进程参数列表中，同机其他用户可读。见 [SECURITY.md](../SECURITY.md)。
- Orb 余额来自非官方 portal API（`portal.withorb.com/api/v1`），不来自 Zed，也没有公开契约。它可能随时变化或失效，届时解析会返回错误码而不是余额。余额按账号缓存 5 分钟，因此最多滞后 portal 这么久；失败的获取缓存 60 秒。
- Orb 链接是放在 URL 查询串里的 bearer token，拿到它的人可以读取对应客户的 Orb 数据，请当作密码对待。它以明文存放在 `accounts.json`，API 不会返回它，日志也不会写入它。
