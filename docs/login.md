[← Back to README](../README.md)

# Login

Login creates or updates `accounts.json` in the data directory. Zed's login flow sends the browser back to a callback port on the machine that started the login. In Docker that port is not reachable from the browser, so use the paste flow.

## Docker: paste callback

1. Open `http://127.0.0.1:8001/` and open the accounts page.
2. Click the button that adds a Zed account.
3. Click the authorization link and finish signing in on the Zed page.
4. The browser lands on `http://127.0.0.1:<ZED_LOGIN_PORT>/...`. This page not loading is expected inside the container.
5. Copy the complete URL from the address bar, paste it into the input on the accounts page, and submit. The account list refreshes with the new account.

`ZED_LOGIN_PORT` defaults to `58443`. It is only embedded in the authorization URL to form the callback address; nothing needs to listen on it in this mode. The authorization link is valid for 10 minutes, the RSA private key stays in process memory, one authorization maps to one login, and only one login can be in progress at a time.

## Host: command line

```sh
zig build
./zig-out/bin/zed-proxy login my-account
```

The command opens the authorization URL in the default browser, listens on a random loopback port for the callback, and saves the account as `my-account` when it arrives. The wait is bounded to 10 minutes.

## Local callback mode through the server

`POST /zed/login` without a `mode` field starts the same local callback flow inside the running server: it listens on a random `127.0.0.1` port, opens the browser, and waits up to 10 minutes. The web page always uses the paste flow, so this mode is useful only when the callback port is reachable, for example when the server runs directly on the host.
