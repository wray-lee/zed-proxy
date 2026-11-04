[← Back to README](../README.md)

# Authentication

Gateway keys are read in this order, and the first source that produces a non-empty value wins:

1. `ZED_API_KEY` environment variable
2. `ZED_API_KEY_FILE` file
3. `<data dir>/api_key` (`/data/api_key` in the container)

Values are comma or newline separated; surrounding whitespace and empty items are dropped. Multiple keys can be configured at once. A file that cannot be read yields no keys; there is no fallback to the next source after a read failure.

Protected paths are those starting with `/v1/` or `/zed/`. A request to a protected path must carry either header:

```
Authorization: Bearer <key>
x-api-key: <key>
```

`/health` and `/` are public. So is `/api/event_logging/batch`, because it sits outside the two protected prefixes; it acknowledges client event batches and stores nothing. Browser `OPTIONS` preflight requests are also answered without a key.

If no key is configured at all and the service listens on loopback, authentication is disabled. This keeps local-only use as before. If the bind address is not loopback and no key is configured, the process prints a refusal message and exits with status 1 instead of exposing an unauthenticated proxy.
