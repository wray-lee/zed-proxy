//! `serve`: bind, print the startup diagnostics and run the axum service
//! (spec `75-config-and-cli.md` §2.1; spec `55-gateway-auth.md` §4.2).
//!
//! The connection loop is hand-rolled instead of `axum::serve` because hyper
//! always chunks an unknown-length body and always appends `date:`; the
//! gateway contract is `Connection: close` framing and no `Date`/`Server`
//! header (spec 16 §1, spec 70 §3). Requests are parsed here, dispatched
//! through the axum router, and the response is written with an exact
//! `Content-Length` when the body has one, close-delimited otherwise.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, Request, StatusCode, Version};
use axum::Router;
use futures_util::StreamExt;
use http_body::Body as _;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tower::ServiceExt;

use crate::accounts::AccountStore;
use crate::catalog::{Catalog, CatalogHandle};
use crate::config::{self, Config};
use crate::upstream::HttpUpstreamClient;

use super::auth::{self, BindDecision, GatewayKeys};
use super::body;
use super::router::{build_router, AppState};

/// Request heads are bounded while reading (spec 70 has no explicit cap; the
/// 16 MiB body cap is enforced by the entry guard).
const MAX_HEAD_BYTES: usize = 64 * 1024;

/// Unix seconds; the single clock helper used by handlers.
pub fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

/// `serve [port]`. Returns the process exit code (0 on clean shutdown).
pub fn run_serve(port_arg: Option<&str>) -> i32 {
    let config = match Config::from_env() {
        Ok(config) => config,
        Err(err) => {
            eprintln!("[zed-proxy] {err}");
            return 1;
        }
    };
    if let Err(err) = config::prepare_data_dir(config.data_dir.as_ref()) {
        eprintln!("[zed-proxy] {err}");
        return 1;
    }

    let keys = GatewayKeys::from_env();
    match auth::bind_decision(config.is_loopback(), keys.len()) {
        BindDecision::Refuse => {
            // Exact exit code and message shape are part of the spec.
            eprintln!("[zed-proxy] {}", auth::REFUSAL_MESSAGE);
            1
        }
        BindDecision::Allow { auth_enabled } => {
            let port = match port_arg {
                Some(value) => config::resolve_serve_port(Some(value), None),
                None => config.port,
            };
            let data_dir = config
                .data_dir
                .clone()
                .unwrap_or_else(|| std::path::PathBuf::from("."));
            run(config, port, keys, auth_enabled, data_dir)
        }
    }
}

fn run(
    config: Config,
    port: u16,
    keys: GatewayKeys,
    auth_enabled: bool,
    data_dir: std::path::PathBuf,
) -> i32 {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            eprintln!("[zed-proxy] cannot start async runtime: {err}");
            return 1;
        }
    };
    runtime.block_on(async move {
        let catalog = match Catalog::builtin() {
            Ok(catalog) => catalog,
            Err(err) => {
                eprintln!("[zed-proxy] {err}");
                return 1;
            }
        };
        let account_count = {
            let store = AccountStore::load(data_dir.clone(), now_unix());
            store.len()
        };
        let upstream = match HttpUpstreamClient::new(config.upstream.clone(), config.proxy.as_ref())
        {
            Ok(client) => client,
            Err(err) => {
                eprintln!("[zed-proxy] {err}");
                return 1;
            }
        };
        let services = Arc::new(crate::proxy::Services {
            catalog: CatalogHandle::new(catalog),
            accounts: AccountStore::load(data_dir, now_unix()),
            upstream: Box::new(upstream),
        });
        let state = AppState {
            services,
            keys: Arc::new(keys),
        };
        let app = build_router(state);

        let bind_host = config.bind_host().to_string();
        let listener = match TcpListener::bind((bind_host.as_str(), port)).await {
            Ok(listener) => listener,
            Err(err) => {
                eprintln!("[zed-proxy] cannot bind {bind_host}:{port}: {err}");
                return 1;
            }
        };
        let local = listener
            .local_addr()
            .map(|addr| addr.to_string())
            .unwrap_or_else(|_| format!("{bind_host}:{port}"));
        print_startup_diagnostics(&config, &local, account_count, auth_enabled);

        loop {
            let (socket, _peer) = match listener.accept().await {
                Ok(accepted) => accepted,
                Err(err) => {
                    eprintln!("[zed-proxy] accept failed: {err}");
                    continue;
                }
            };
            // One request per connection (spec 70 §1.7).
            let app = app.clone();
            tokio::spawn(async move {
                serve_connection(socket, app).await;
            });
        }
    })
}

/// One request and one response, then close (spec 70 §1.7). A client that
/// never finishes its head is dropped after 30 seconds.
async fn serve_connection(mut socket: TcpStream, app: Router) {
    let read = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        read_request(&mut socket),
    )
    .await;
    let Some((head, payload)) = read.ok().flatten() else {
        return;
    };
    let request = head.into_request(payload);
    let response = match app.oneshot(request).await {
        Ok(response) => response,
        Err(err) => match err {},
    };
    if write_response(&mut socket, response).await.is_ok() {
        let _ = socket.flush().await;
    }
    let _ = socket.shutdown().await;
}

/// A parsed request head plus the URL it targets.
struct RequestHead {
    method: Method,
    uri: String,
    version: Version,
    headers: HeaderMap,
}

impl RequestHead {
    fn into_request(self, payload: Vec<u8>) -> Request<Body> {
        let mut request = Request::new(Body::from(payload));
        *request.method_mut() = self.method;
        if let Ok(uri) = self.uri.parse() {
            *request.uri_mut() = uri;
        }
        *request.version_mut() = self.version;
        *request.headers_mut() = self.headers;
        request
    }
}

/// Read one request head and its `Content-Length` body. `None` means the
/// connection closed or sent something unparseable. A chunked request body is
/// not supported: the body is treated as empty, which the entry layer turns
/// into a 400 (spec 55 §3.1).
async fn read_request(socket: &mut TcpStream) -> Option<(RequestHead, Vec<u8>)> {
    let mut buffer: Vec<u8> = Vec::with_capacity(1024);
    let end = loop {
        if let Some(position) = find_head_end(&buffer) {
            break position;
        }
        if buffer.len() > MAX_HEAD_BYTES {
            return None;
        }
        let mut chunk = [0u8; 4096];
        let read = socket.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
    };

    let head_text = String::from_utf8_lossy(&buffer[..end]).into_owned();
    let mut lines = head_text.split("\r\n");
    let request_line = lines.next()?;
    let mut parts = request_line.split(' ');
    let method = Method::from_bytes(parts.next()?.as_bytes()).ok()?;
    let uri = parts.next()?.to_string();
    let version = match parts.next()? {
        "HTTP/1.0" => Version::HTTP_10,
        _ => Version::HTTP_11,
    };

    let mut headers = HeaderMap::new();
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let name = HeaderName::from_bytes(name.trim().as_bytes()).ok();
        let value = HeaderValue::from_str(value.trim()).ok();
        if let (Some(name), Some(value)) = (name, value) {
            headers.append(name, value);
        }
    }

    // The gate rejects an oversized body on the header alone; never read it.
    let declared = headers
        .get(axum::http::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok())
        .unwrap_or(0);
    if headers.contains_key(axum::http::header::TRANSFER_ENCODING)
        || declared > body::MAX_REQUEST_BODY
    {
        return Some((
            RequestHead {
                method,
                uri,
                version,
                headers,
            },
            Vec::new(),
        ));
    }

    let mut payload = buffer[end + 4..].to_vec();
    let want = declared as usize;
    while payload.len() < want {
        let mut chunk = vec![0u8; (want - payload.len()).min(64 * 1024)];
        let read = socket.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        payload.extend_from_slice(&chunk[..read]);
    }
    payload.truncate(want);
    Some((
        RequestHead {
            method,
            uri,
            version,
            headers,
        },
        payload,
    ))
}

fn find_head_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|window| window == b"\r\n\r\n")
}

/// Write the status line, headers and body. A body of known length gets an
/// exact `Content-Length`; a stream is close-delimited, never chunked
/// (spec 16 §1). `Date`, `Server` and `Transfer-Encoding` are never written
/// (spec 70 §3).
async fn write_response(
    socket: &mut TcpStream,
    response: axum::response::Response,
) -> std::io::Result<()> {
    let (parts, body) = response.into_parts();
    let status = parts.status;
    let mut head = String::with_capacity(256);
    head.push_str("HTTP/1.1 ");
    head.push_str(status.as_str());
    head.push(' ');
    head.push_str(status.canonical_reason().unwrap_or("Unknown"));
    head.push_str("\r\n");
    for (name, value) in parts.headers.iter() {
        if name == axum::http::header::CONTENT_LENGTH
            || name == axum::http::header::TRANSFER_ENCODING
            || name == axum::http::header::DATE
            || name == axum::http::header::SERVER
        {
            continue;
        }
        if let Ok(text) = value.to_str() {
            head.push_str(name.as_str());
            head.push_str(": ");
            head.push_str(text);
            head.push_str("\r\n");
        }
    }

    let exact = body.size_hint().exact();
    let mut stream = body.into_data_stream();
    if let Some(length) = exact {
        head.push_str("content-length: ");
        head.push_str(&length.to_string());
        head.push_str("\r\n\r\n");
        socket.write_all(head.as_bytes()).await?;
        while let Some(frame) = stream.next().await {
            if let Ok(data) = frame {
                socket.write_all(&data).await?;
            }
        }
    } else {
        head.push_str("\r\n");
        socket.write_all(head.as_bytes()).await?;
        while let Some(frame) = stream.next().await {
            if let Ok(data) = frame {
                socket.write_all(&data).await?;
                socket.flush().await?;
            }
        }
    }
    Ok(())
}

fn print_startup_diagnostics(
    config: &Config,
    local: &str,
    account_count: usize,
    auth_enabled: bool,
) {
    eprintln!("[zed-proxy] http://{local}");
    eprintln!("[zed-proxy] {account_count} account(s) loaded");
    eprintln!(
        "[zed-proxy] auth: {}",
        if auth_enabled { "on" } else { "off" }
    );
    match &config.proxy {
        Some(proxy) => eprintln!("[zed-proxy] proxy: {}:{}", proxy.host, proxy.port),
        None => eprintln!("[zed-proxy] proxy: none (set HTTPS_PROXY to use)"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_is_in_unix_seconds() {
        let now = now_unix();
        // 2020-01-01 .. 2100-01-01
        assert!(now > 1_577_836_800 && now < 4_102_444_800);
    }

    #[test]
    fn head_end_is_found_at_the_blank_line() {
        assert_eq!(
            find_head_end(b"GET / HTTP/1.1\r\nA: b\r\n\r\nrest"),
            Some(20)
        );
        assert_eq!(find_head_end(b"GET / HTTP/1.1\r\n"), None);
    }

    #[tokio::test]
    async fn a_request_head_and_body_are_split() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let client = tokio::spawn(async move {
            let mut socket = TcpStream::connect(address).await.unwrap();
            socket
                .write_all(b"POST /v1/chat/completions HTTP/1.1\r\nHost: x\r\nContent-Length: 7\r\n\r\n{\"a\":1}")
                .await
                .unwrap();
            socket
        });
        let (mut server, _) = listener.accept().await.unwrap();
        let (head, payload) = read_request(&mut server).await.unwrap();
        assert_eq!(head.method, Method::POST);
        assert_eq!(head.uri, "/v1/chat/completions");
        assert_eq!(payload, b"{\"a\":1}");
        drop(client);
    }

    #[tokio::test]
    async fn a_chunked_body_is_treated_as_empty() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let writer = tokio::spawn(async move {
            let mut socket = TcpStream::connect(address).await.unwrap();
            socket
                .write_all(
                    b"POST /v1/chat/completions HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\n7\r\n{\"a\":1}\r\n0\r\n\r\n",
                )
                .await
                .unwrap();
            socket
        });
        let (mut server, _) = listener.accept().await.unwrap();
        let (_head, payload) = read_request(&mut server).await.unwrap();
        assert!(payload.is_empty());
        drop(writer);
    }
}
