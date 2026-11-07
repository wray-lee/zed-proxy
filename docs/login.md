[← Back to README](../README.md)

# Login

Login creates or updates `accounts.json` in the data directory. Zed's login flow sends the browser back to a callback port on the machine that started the login. In Docker that port is not reachable from the browser, so use the paste flow.

## Docker: paste callback

1. Open `http://127.0.0.1:8001/`; the accounts page is shown by default (or open `#accounts`).
2. Click "Add account". The login panel opens and asks the server for an authorization link.
3. Click "Open the authorization page in Zed" and finish signing in on the Zed page.
4. The browser lands on `http://127.0.0.1:<ZED_LOGIN_PORT>/...`. This page not loading is expected inside the container.
5. Copy the complete URL from the address bar, paste it into "Paste the callback URL" on the accounts page, and submit. The account list refreshes with the new account.

`ZED_LOGIN_PORT` defaults to `58443`. It is only embedded in the authorization URL to form the callback address; nothing needs to listen on it in this mode. The authorization link is valid for 10 minutes, the RSA private key stays in process memory, one authorization maps to one login, and only one login can be in progress at a time. Nothing opens a browser: the link is returned in the response and shown in the page.

The private key is generated and used in-process (RSA-OAEP-SHA256); no `openssl` command and no temporary ciphertext file are involved.

## Host: command line

```sh
cargo build --release
./target/release/zed-proxy login my-account
```

The command prints the authorization URL (it does not open a browser), listens on a random loopback port for the callback, and saves the account as `my-account` when it arrives. The wait is bounded to 10 minutes. With no name argument the account is stored under the callback's `user_id`.

## Local callback mode through the server

`POST /zed/login` without a `mode` field starts the same local callback flow inside the running server: it listens on a random `127.0.0.1` port and waits up to 10 minutes. The web page always uses the paste flow, so this mode is useful only when the callback port is reachable, for example when the server runs directly on the host.

## Callback parsing

A pasted value may be the full callback URL or the bare query string, with or without a leading `?`. Everything after `#` is dropped. `%XX` and `+` are decoded. If a parameter appears twice the last value wins. `user_id` is checked before `access_token`, and both must be non-empty after decoding. Input longer than 8192 bytes is rejected. Anything that is neither a URL with a query string nor a `k=v` list is rejected as "not a callback". Decryption failures (bad base64, wrong length, wrong padding, wrong key) all produce the same message so the endpoint cannot be used as a padding oracle.
