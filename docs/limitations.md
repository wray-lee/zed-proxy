[← Back to README](../README.md)

# Known limitations

- The paste callback decrypts only with RSA-OAEP-SHA256. OmniRoute's Zed helper also falls back to RSA PKCS#1 v1.5. Which padding the real Zed callback uses is not verified with a real account.
- The web page offers only the paste flow. The local callback listener still exists and can be triggered with `POST /zed/login` or the CLI.
- Nothing opens a browser. `zed-proxy login` and `POST /zed/login` print or return the authorization URL instead, so the user must open it manually. 0.2.2 tried `xdg-open` and friends.
- Real Zed login callbacks and real `gemini-4-argon` calls are not verified with a real account. Every test and probe so far used constructed credentials or local fakes.
- The arm64 image was built and started only under QEMU. It has not run on real arm64 hardware.
- `ZED_UPSTREAM_BASE` exists so the proxy can be pointed at a fake upstream for differential tests. It has only ever been used against local fakes; the real `cloud.zed.dev` behaviour with a custom prefix is unverified.
- The 0.3.1 build has been exercised against `cloud.zed.dev` with a real account: three providers through all three client protocols, tool round trips and parallel tool calls, `apply_patch` custom tools, and the Gemini thought-signature replay (see CHANGELOG 0.3.1). The points below are the ones a real account could not settle.
- Gemini tool calling is now verified on the real upstream for schema cleaning, the tool-call round trips (all three client protocols), parallel calls and `thoughtSignature` replay. Still unverified: `inlineData` images, `toolConfig`/`functionCallingConfig`, and whether the upstream accepts the `generationConfig.maxOutputTokens` the proxy sends.
- The output budget of the OpenAI provider is clamped to the catalogue maximum (for example 999999 becomes 128000). Only the local fake accepted or rejected this; whether Zed's real upstream caps the same way is unverified.
- The Gemini `systemInstruction` mapping and the `thinkingLevel` mapping are covered by local tests only.
- `/v1/responses` to Gemini models is implemented but each such cell of the conversion matrix is only checked against the fake upstream.
- The proxy is single-user. There is no per-client quota, rate limit, or tenant isolation.
- Upstream requests imitate the Zed client (`user-agent: Zed/1.8.2 (linux; x86_64)`), and the upstream protocol can change at any time. Long-term availability is not guaranteed.
- `ZED_VERSION` is fixed at compile time (`1.8.2`). A different upstream client version may change which models or plans are accepted.
- No system id is sent when minting tokens unless `ZED_SYSTEM_ID` is set. A plain account minted tokens and completed requests without it in the author's checks; if Zed answers a trial account with `trial_blocked`, set `ZED_SYSTEM_ID` to the id your Zed client registered under.
- `GET /zed/usage` and `GET /zed/billing` are management endpoints over the real account: the first mints (or reuses) an LLM token and returns only its JWT payload, byte for byte as the token carries it, the second proxies `client/users/me` verbatim, answering 502 on any upstream failure. Both answer 400 `{"error":"no account"}` without an active account. They have only been exercised against a local fake upstream; the Web UI does not call them.
- The Orb credit balance comes from an unofficial portal API (`portal.withorb.com/api/v1`), not from Zed and not from a documented contract. It can change or stop working without notice, and the response parsing may then report an error code instead of a balance. The balance is cached per account for 5 minutes (60 seconds after a failure), and a forced refresh is limited to once per account every 5 seconds, so it can lag behind the portal.
- The Orb link is a bearer token in the URL query string. Anyone who obtains it can read that customer's Orb data, so treat it like a password. It is stored in `accounts.json` in plaintext and is never returned by the API or written to a log.
- A successful stream is recorded as soon as the first model event is committed; a failure after that point (or a client disconnect) is not charged to the account.
- `accounts.json` is capped at 4 MiB and account names/`user_id` at 256 bytes; a login beyond those limits is rejected.
