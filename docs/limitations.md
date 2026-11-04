[← Back to README](../README.md)

# Known limitations

- The paste callback decrypts only with RSA-OAEP-SHA256. OmniRoute's Zed helper also falls back to RSA PKCS#1 v1.5. Which padding the real Zed callback uses is not verified with a real account.
- The web page offers only the paste flow. The local callback listener still exists and can be triggered with `POST /zed/login` or the CLI.
- Real Zed login callbacks and real `gemini-4-argon` calls are not verified with a real account. Every test and probe so far used constructed credentials or local fakes.
- The arm64 image was built and started only under QEMU. It has not run on real arm64 hardware.
- The Windows BCrypt login branch compiles but has not been run.
- `/v1/responses` does not carry Gemini models; only Chat Completions and Anthropic Messages route to Google.
- The proxy is single-user. There is no per-client quota, rate limit, or tenant isolation.
- Upstream requests imitate the Zed client, and the upstream protocol can change at any time. Long-term availability is not guaranteed.
- `ZED_VERSION` is fixed at compile time (`1.8.2`). A different upstream client version may change which models or plans are accepted.
- No system id is sent when minting tokens unless `ZED_SYSTEM_ID` is set. A plain account minted tokens and completed requests without it in the author's checks; if Zed answers a trial account with `trial_blocked`, set `ZED_SYSTEM_ID` to the id your Zed client registered under.
- Upstream credentials are passed to `curl` as `-H` arguments, so they appear in the process argument list and can be read by other users on the same host. See [SECURITY.md](../SECURITY.md).
