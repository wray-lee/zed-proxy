[← 返回 README](../README.zh-CN.md)

# 非官方声明与条款风险

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
