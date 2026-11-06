//! `serve`: bind, print the startup diagnostics and run the axum service
//! (spec `75-config-and-cli.md` §2.1; spec `55-gateway-auth.md` §4.2).
//!
//! The connection loop is hand-rolled instead of `axum::serve` because hyper
//! always chunks an unknown-length body and always appends `date:`; the
//! gateway contract is `Connection: close` framing and no `Date`/`Server`
//! header (spec 16 §1, spec 70 §3). Requests are parsed here, dispatched
//! through the axum router, and the response is written with an exact
//! `Content-Length` when the body has one, close-delimited otherwise.

use std::future::Future;
use std::io;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, Request, Version};
use axum::Router;
use futures_util::StreamExt;
use http_body::Body as _;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;
use tower::ServiceExt;

use crate::accounts::AccountStore;
use crate::catalog::{Catalog, CatalogHandle};
use crate::config::{self, Config};
use crate::upstream::HttpUpstreamClient;

use super::auth::{self, BindDecision, GatewayKeys};
use super::body;
use super::router::{build_router, AppState};

/// Header block cap (spec 55 §3.1): 65536 bytes, no response when exceeded.
const MAX_HEAD_BYTES: usize = 65_536;

/// Conservative cap on concurrently handled connections: one request per
/// connection holds a task, a socket and up to 64 KiB of head buffer. 256 is
/// well above the handful of simultaneous streams a single-workstation
/// gateway serves, yet stays below the common 1024 fd soft limit so the
/// listener, upstream sockets and files still fit. It bounds fd/memory growth
/// from slow clients; excess sockets are dropped immediately, never queued.
const MAX_CONNECTIONS: usize = 256;

/// Matches the 300 s upstream stream timeout (spec 16 §3). It bounds the
/// non-writing phases (router/oneshot) and each individual write/flush, but is
/// never a total-duration cap on a streaming response: a stream that keeps
/// producing bytes may run longer.
const IDLE_TIMEOUT: Duration = Duration::from_secs(300);

/// Short stalls for the self-tests; production uses [`IDLE_TIMEOUT`].
#[cfg(test)]
const TEST_WRITE_STALL: Duration = Duration::from_millis(150);

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

        let connections = Arc::new(Semaphore::new(MAX_CONNECTIONS));
        loop {
            let (socket, _peer) = match listener.accept().await {
                Ok(accepted) => accepted,
                Err(err) => {
                    eprintln!("[zed-proxy] accept failed: {err}");
                    continue;
                }
            };
            // One request per connection (spec 70 §1.7). No queue: an
            // exhausted slot closes this socket immediately.
            try_spawn_connection(&connections, socket, app.clone(), IDLE_TIMEOUT);
        }
    })
}

/// One request and one response, then close (spec 70 §1.7). A client that
/// never finishes its head is dropped after 30 seconds.
async fn serve_connection(mut socket: TcpStream, app: Router, write_stall: Duration) {
    let read = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        read_request(&mut socket),
    )
    .await;
    let Some((head, payload)) = read.ok().flatten() else {
        return;
    };
    let request = head.into_request(payload);
    // Bounds the pre-write phase only. A committed streaming response returns
    // from `oneshot` as soon as its headers exist, so its body is not capped
    // by this timeout.
    let response = match tokio::time::timeout(IDLE_TIMEOUT, app.oneshot(request)).await {
        Ok(Ok(response)) => response,
        Ok(Err(err)) => match err {},
        Err(_) => return,
    };
    let bounded = response.body().size_hint().exact().is_some();
    let writing = write_response(&mut socket, response, write_stall);
    // Known-length bodies also get a total cap; streaming bodies get only the
    // per-write/per-frame stall cap inside `write_response`, so a long SSE that
    // keeps producing bytes is never cut off by elapsed wall time.
    let finished = if bounded {
        idle(IDLE_TIMEOUT, writing).await.and_then(|result| result)
    } else {
        writing.await
    };
    if finished.is_ok() {
        let _ = idle(write_stall, socket.flush()).await;
    }
    let _ = socket.shutdown().await;
}

fn try_spawn_connection(
    connections: &Arc<Semaphore>,
    socket: TcpStream,
    app: Router,
    write_stall: Duration,
) {
    // The owned permit holds an Arc to the semaphore across the whole spawn:
    // read, oneshot, response writes and shutdown all count against the cap.
    // A failed try-acquire is never queued; the accepted socket is dropped.
    let permit = match Arc::clone(connections).try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => {
            drop(socket);
            return;
        }
    };
    tokio::spawn(async move {
        let _permit = permit;
        serve_connection(socket, app, write_stall).await;
    });
}

fn stalled() -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, "idle connection")
}

async fn idle<T, F>(stall: Duration, future: F) -> io::Result<T>
where
    F: Future<Output = T>,
{
    tokio::time::timeout(stall, future)
        .await
        .map_err(|_| stalled())
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
            // The whole head (including its terminator) must fit the cap; bytes
            // past it belong to the body and are kept by `read_request`.
            if position + 4 > MAX_HEAD_BYTES {
                return None;
            }
            break position;
        }
        if buffer.len() >= MAX_HEAD_BYTES {
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
    stall: Duration,
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
        idle(stall, socket.write_all(head.as_bytes())).await??;
        while let Some(frame) = idle(stall, stream.next()).await? {
            if let Ok(data) = frame {
                idle(stall, socket.write_all(&data)).await??;
            }
        }
    } else {
        head.push_str("\r\n");
        idle(stall, socket.write_all(head.as_bytes())).await??;
        while let Some(frame) = idle(stall, stream.next()).await? {
            if let Ok(data) = frame {
                idle(stall, socket.write_all(&data)).await??;
                idle(stall, socket.flush()).await??;
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
    use axum::body::Bytes;
    use axum::response::Response;
    use axum::routing::any;
    use futures_util::stream;

    use super::*;
    use crate::catalog::Catalog;
    use crate::error::GatewayError;
    use crate::http::auth::GatewayKeys;
    use crate::http::router::AppState;
    use crate::proxy::Services;
    use crate::upstream::client::{BoxFuture, UpstreamRequest, UpstreamResponse, UpstreamStream};
    use crate::upstream::UpstreamClient;

    struct NoUpstream;

    impl UpstreamClient for NoUpstream {
        fn execute<'a>(
            &'a self,
            _: UpstreamRequest,
        ) -> BoxFuture<'a, Result<UpstreamResponse, GatewayError>> {
            Box::pin(async { Err(GatewayError::internal("not used")) })
        }

        fn execute_stream<'a>(
            &'a self,
            _: UpstreamRequest,
        ) -> BoxFuture<'a, Result<UpstreamStream, GatewayError>> {
            Box::pin(async { Err(GatewayError::internal("not used")) })
        }
    }

    fn test_app() -> Router {
        // A nonexistent data dir loads an empty account store and creates no files.
        let data_dir = std::env::temp_dir().join(format!(
            "zprs-http-missing-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        build_router(AppState {
            services: Arc::new(Services {
                catalog: CatalogHandle::new(Catalog::default()),
                accounts: AccountStore::load(data_dir, 0),
                upstream: Box::new(NoUpstream),
            }),
            keys: Arc::new(GatewayKeys::default()),
        })
    }

    /// An unknown-length body that emits one byte every `delay`. This is the
    /// shape of a long SSE response; it must survive past `stall` in total.
    fn slow_app(frames: usize, delay: Duration) -> Router {
        Router::new().route(
            "/slow",
            any(move || {
                let (frames, delay) = (frames, delay);
                async move {
                    let body = stream::unfold(0usize, move |sent| async move {
                        if sent == frames {
                            return None;
                        }
                        tokio::time::sleep(delay).await;
                        Some((Ok::<_, GatewayError>(Bytes::from_static(b"x")), sent + 1))
                    });
                    Response::new(axum::body::Body::from_stream(body))
                }
            }),
        )
    }

    /// A known-length body big enough to fill a loopback receive window.
    fn large_app() -> Router {
        Router::new().route(
            "/large",
            any(|| async { Response::new(Body::from(vec![b'x'; 8 * 1024 * 1024])) }),
        )
    }

    async fn wait_for_permits(gate: &Arc<Semaphore>, want: usize) {
        for _ in 0..200 {
            if gate.available_permits() == want {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(gate.available_permits(), want);
    }

    async fn start_server(
        app: Router,
        stall: Duration,
        slots: usize,
    ) -> (std::net::SocketAddr, Arc<Semaphore>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let connections = Arc::new(Semaphore::new(slots));
        let gate = Arc::clone(&connections);
        tokio::spawn(async move {
            loop {
                let (socket, _) = listener.accept().await.unwrap();
                try_spawn_connection(&gate, socket, app.clone(), stall);
            }
        });
        (address, connections)
    }

    async fn raw_request(address: std::net::SocketAddr, head: &str, body: &[u8]) -> Vec<u8> {
        let mut socket = TcpStream::connect(address).await.unwrap();
        socket.write_all(head.as_bytes()).await.unwrap();
        if !body.is_empty() {
            socket.write_all(body).await.unwrap();
        }
        let mut reply = Vec::new();
        // A dropped connection can surface as ECONNRESET; that still means
        // "no response", which is what the spec requires for oversized heads.
        let _ = socket.read_to_end(&mut reply).await;
        reply
    }

    fn parse_head(reply: &[u8]) -> (String, String, Vec<u8>) {
        let split = reply
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .expect("response head");
        let head = String::from_utf8_lossy(&reply[..split + 4]).into_owned();
        let status = head.lines().next().unwrap().to_string();
        let content_type = head
            .lines()
            .find_map(|line| {
                line.strip_prefix("content-type: ")
                    .or_else(|| line.strip_prefix("Content-Type: "))
            })
            .unwrap_or("")
            .to_string();
        (status, content_type, reply[split + 4..].to_vec())
    }

    fn request_head(path: &str, length: usize) -> String {
        format!("POST {path} HTTP/1.1\r\nHost: localhost\r\nContent-Length: {length}\r\n\r\n")
    }

    fn json_payload(size: usize) -> Vec<u8> {
        // `{"padding":"..."}` is a valid JSON object that reaches the handler.
        let mut bytes = Vec::with_capacity(size);
        bytes.extend_from_slice(br#"{"padding":""#);
        bytes.resize(size - 2, b'x');
        bytes.extend_from_slice(br#""}"#);
        assert_eq!(bytes.len(), size);
        bytes
    }

    #[tokio::test]
    async fn oversize_head_is_closed_without_a_response() {
        let (address, _gate) = start_server(test_app(), IDLE_TIMEOUT, MAX_CONNECTIONS).await;
        let mut socket = TcpStream::connect(address).await.unwrap();
        let mut head = b"GET / HTTP/1.1\r\nX: ".to_vec();
        head.resize(MAX_HEAD_BYTES + 64, b'a');
        head.extend_from_slice(b"\r\n\r\n");
        socket.write_all(&head).await.unwrap();
        let mut reply = Vec::new();
        // A dropped socket can surface as ECONNRESET; either way there is no
        // HTTP response, which is what spec 55 §3.1 requires.
        let _ = socket.read_to_end(&mut reply).await;
        assert!(reply.is_empty(), "expected no response, got {reply:?}");
    }

    #[tokio::test]
    async fn a_head_at_the_exact_limit_is_accepted() {
        assert_eq!(MAX_HEAD_BYTES, 65_536);
        let (address, _gate) = start_server(test_app(), IDLE_TIMEOUT, MAX_CONNECTIONS).await;
        let mut socket = TcpStream::connect(address).await.unwrap();
        let mut head = b"GET / HTTP/1.1\r\nX: ".to_vec();
        head.resize(MAX_HEAD_BYTES - 4, b'a');
        head.extend_from_slice(b"\r\n\r\n");
        assert_eq!(head.len(), MAX_HEAD_BYTES);
        socket.write_all(&head).await.unwrap();
        let mut reply = Vec::new();
        socket.read_to_end(&mut reply).await.unwrap();
        assert!(
            reply.starts_with(b"HTTP/1.1 200 "),
            "{:?}",
            &reply[..40.min(reply.len())]
        );
    }

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

    #[tokio::test]
    async fn a_body_just_over_2_mib_is_not_rejected_by_default_body_limit() {
        let (address, _gate) = start_server(test_app(), IDLE_TIMEOUT, MAX_CONNECTIONS).await;
        let size = 2_097_153;
        // `/zed/models/refresh` is a real `Bytes` handler: a valid JSON object
        // reaches the handler and, with no accounts, answers 200.
        let reply = raw_request(
            address,
            &request_head("/zed/models/refresh", size),
            &json_payload(size),
        )
        .await;
        let (status, _, _) = parse_head(&reply);
        assert!(
            status.starts_with("HTTP/1.1 200"),
            "unexpected status: {status}"
        );
    }

    #[tokio::test]
    async fn a_body_over_16_mib_gets_the_exact_json_413() {
        let (address, _gate) = start_server(test_app(), IDLE_TIMEOUT, MAX_CONNECTIONS).await;
        let size = 16_777_217;
        // The entry guard answers from the header alone; never send the body.
        let mut socket = TcpStream::connect(address).await.unwrap();
        socket
            .write_all(request_head("/zed/accounts/orb", size).as_bytes())
            .await
            .unwrap();
        let mut reply = Vec::new();
        socket.read_to_end(&mut reply).await.unwrap();
        let (status, content_type, bytes) = parse_head(&reply);
        assert!(
            status.starts_with("HTTP/1.1 413"),
            "unexpected status: {status}"
        );
        assert_eq!(content_type, "application/json");
        assert_eq!(bytes, body::BODY_TOO_LARGE.as_bytes());
    }

    #[tokio::test]
    async fn a_body_at_exactly_16_mib_is_not_a_413() {
        let (address, _gate) = start_server(test_app(), IDLE_TIMEOUT, MAX_CONNECTIONS).await;
        let size = 16_777_216;
        let reply = raw_request(
            address,
            &request_head("/zed/models/refresh", size),
            &json_payload(size),
        )
        .await;
        let (status, _, _) = parse_head(&reply);
        assert!(
            status.starts_with("HTTP/1.1 200"),
            "16 MiB must be read, got {status}"
        );
    }

    #[tokio::test]
    async fn the_connection_cap_drops_excess_sockets() {
        let (address, gate) = start_server(test_app(), IDLE_TIMEOUT, 1).await;
        // An incomplete head keeps the first connection alive: the 30 s read
        // timeout is still ticking and the permit is held for the whole task.
        let mut held = TcpStream::connect(address).await.unwrap();
        held.write_all(b"GET / HTTP/1.1\r\nHost: x\r\n")
            .await
            .unwrap();
        wait_for_permits(&gate, 0).await;
        let mut extra = TcpStream::connect(address).await.unwrap();
        let mut reply = Vec::new();
        tokio::time::timeout(Duration::from_secs(2), extra.read_to_end(&mut reply))
            .await
            .unwrap()
            .ok();
        assert!(
            reply.is_empty(),
            "excess connection got a response: {reply:?}"
        );
    }

    #[tokio::test]
    async fn a_write_stall_drops_the_connection_but_not_an_active_stream() {
        // 6 frames at 60 ms each = 360 ms total, past the 150 ms stall. Every
        // single write/frame is faster than the stall, so the long stream must
        // complete; a response that stops producing frames must be reclaimed.
        let frames = 6;
        let (address, gate) = start_server(
            slow_app(frames, Duration::from_millis(60)),
            TEST_WRITE_STALL,
            2,
        )
        .await;
        let reply = raw_request(address, "GET /slow HTTP/1.1\r\nHost: x\r\n\r\n", &[]).await;
        let (status, _, bytes) = parse_head(&reply);
        assert!(
            status.starts_with("HTTP/1.1 200"),
            "unexpected status: {status}"
        );
        assert_eq!(
            bytes.len(),
            frames,
            "long stream was cut by a total timeout"
        );
        wait_for_permits(&gate, 2).await;

        // A stream whose body never produces a frame is released by the stall.
        let hanging = Router::new().route(
            "/hang",
            any(|| async {
                Response::new(axum::body::Body::from_stream(stream::pending::<
                    Result<Bytes, GatewayError>,
                >()))
            }),
        );
        let (address, gate) = start_server(hanging, TEST_WRITE_STALL, 1).await;
        let mut socket = TcpStream::connect(address).await.unwrap();
        socket
            .write_all(b"GET /hang HTTP/1.1\r\nHost: x\r\n\r\n")
            .await
            .unwrap();
        let mut reply = Vec::new();
        let _ = socket.read_to_end(&mut reply).await;
        assert!(
            reply.starts_with(b"HTTP/1.1 200"),
            "missing response head: {reply:?}"
        );
        wait_for_permits(&gate, 1).await;
    }

    #[tokio::test]
    async fn a_client_that_stops_reading_is_reclaimed_by_the_write_stall() {
        let (address, gate) = start_server(large_app(), TEST_WRITE_STALL, 1).await;
        // Never drain the socket: 8 MiB fills the loopback receive window and
        // `write_all` blocks until the per-write stall releases the task.
        let mut socket = TcpStream::connect(address).await.unwrap();
        socket
            .write_all(b"GET /large HTTP/1.1\r\nHost: x\r\n\r\n")
            .await
            .unwrap();
        // The task must actually start (permit taken) before we wait for the
        // stall to hand it back.
        tokio::time::sleep(TEST_WRITE_STALL * 2).await;
        wait_for_permits(&gate, 1).await;
    }
}
