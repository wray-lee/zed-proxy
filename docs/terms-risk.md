[← Back to README](../README.md)

# Unofficial project and terms risk

ZedProxy is unofficial. It is not affiliated with, endorsed by, or supported by Zed Industries, Inc. "Zed" is used only to describe the upstream service this proxy talks to.

With an account configured, the proxy sends requests to Zed-operated endpoints on your behalf:

- `cloud.zed.dev/completions` carries `authorization`, `content-type`, `x-zed-version: 1.8.2`, `x-zed-client-supports-status-messages: true`, `x-zed-client-supports-stream-ended-request-completion-status: true`, and `user-agent: Zed/1.8.2 (linux; x86_64)`.
- `cloud.zed.dev/client/llm_tokens` carries `authorization`, `content-type`, `x-zed-system-id` (only when `ZED_SYSTEM_ID` is set), and the same `user-agent`.
- `cloud.zed.dev/models` carries `authorization`, `x-zed-version`, and the same `user-agent`.
- `cloud.zed.dev/client/users/me` carries `authorization`, `accept`, `content-type`, and the same `user-agent`.
- The Orb portal (`portal.withorb.com/api/v1`) receives the browser-like headers described in [Configuration](configuration.md), including a `Referer` that is the link you pasted.
- Login returns `zed.dev/native_app_signin` with a generated public key and callback port. In local callback mode the browser is then redirected to `zed.dev/native_app_signin_succeeded`; in paste mode the callback URL is pasted back into the proxy instead. All four upstream endpoints follow `ZED_UPSTREAM_BASE` when it is set; `zed.dev` never does.

These headers and identifiers exist to make the traffic look like the official client. The account's plan, trial state, and any abuse checks are evaluated against them.

Zed's Terms of Service (`legal/terms.md`, last updated March 2, 2026, published at https://zed.dev/terms) contain clauses relevant to this kind of use. The sections below are cited so you can read them in full; this is a list of citations, not a legal interpretation.

- Section 2.2 (Access Grant): the Service may be used only for internal business purposes or personal non-commercial purposes, in accordance with Zed's documentation, and the customer agrees to access the Service only through the mechanisms designated by Zed.
- Section 2.4(a): the customer will not provide access to, distribute, sell, or sublicense the Service to a third party.
- Section 2.4(b): the customer will not seek to access non-public APIs associated with the Service.
- Section 2.4(d): the customer will not interfere with the operation of the Service, circumvent access restrictions, or conduct security or vulnerability tests of the Service.
- Section 6 (Suspension of Service): Zed may immediately suspend access, including for a breach of Sections 2.2 to 2.4 or Section 5.
- Section 8 (Disclaimers; No Warranties by Zed): the software, service, and output are provided "as is" and "as available", without warranties.
- Section 9.2 (Termination): if the customer violates any provision, the customer is not authorized to access the Service and the Terms terminate automatically; Zed may also suspend or terminate access at any time for any reason, with or without notice.
- Section 12 (Indemnity): the customer will defend and indemnify Zed against claims arising from, among other things, unauthorized use or misuse of the Service, the customer data, use of output, or violation of the Terms.

Section 2.3 also incorporates the acceptable use policies listed at https://zed.dev/acceptable-use-policies, which include the Anthropic Usage Policy, the Google Generative AI Prohibited Use Policy, the OpenAI Usage Policies, and the GitHub Acceptable Use Policies (`legal/third-party-terms.md` in the Zed repository lists the same set).

In practical terms, this project can route your own account's traffic in a way Zed did not intend. Your account may be suspended or terminated, and you use the software at your own risk. Use only your own account, do not share access to your gateway, and do not expose it publicly. This section is not legal advice.
