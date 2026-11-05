//! Proxy orchestration: one client request → account selection → provider
//! encoding → upstream transport → client encoding (spec `00` §3.2,
//! `50` §6–§7, `30` §5–§6).
//!
//! Non-streaming runs the attempt loop inline. Streaming pre-reads the upstream
//! until the first model event, so every failure before the client sees a byte
//! can still be classified, retried (empty 200 streams) or failed over; after
//! that the rest of the connection is handed to [`stream::SseStream`].

pub mod stream;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use futures_util::StreamExt;

use crate::accounts::{AccountSnapshot, AccountStore};
use crate::catalog::{CatalogHandle, ModelCapabilities, Provider};
use crate::client::{ClientEncoder, ClientProtocol};
use crate::error::{ErrorKind, GatewayError};
use crate::neutral::{NeutralRequest, StreamEvent};
use crate::provider::{build_envelope, ProviderEncoder, StreamDecoder};
use crate::upstream::client::{
    backoff_before, classify_attempt, AttemptFailure, Endpoint, UpstreamClient, UpstreamRequest,
    MAX_ATTEMPTS,
};
use crate::upstream::token::{cached_or_minted, TokenCache};
use crate::upstream::ByteStream;

/// Shared services for one process. `Arc`ed into every handler.
pub struct Services {
    pub catalog: CatalogHandle,
    pub accounts: AccountStore,
    pub upstream: Box<dyn UpstreamClient>,
}

/// Outcome of a non-streaming proxy call.
#[derive(Debug)]
pub struct ProxyOutcome {
    pub status: u16,
    pub content_type: &'static str,
    pub body: Vec<u8>,
}

/// Non-streaming proxy request (spec 30 §5).
pub async fn handle_non_stream(
    services: &Services,
    protocol: ClientProtocol,
    body: &serde_json::Value,
) -> Result<ProxyOutcome, GatewayError> {
    let prepared = prepare(services, protocol, body)?;
    if services.accounts.is_empty() {
        return Err(no_account());
    }
    let mut last: Option<GatewayError> = None;
    for name in services.accounts.attempt_order(now_unix()) {
        let Some(account) = services.accounts.snapshot(&name) else {
            continue;
        };
        note_benched(&account);
        match run_account(services, &prepared, &account).await {
            Ok(outcome) => return Ok(outcome),
            Err(err) => {
                // Request-level failures never reach another account
                // (spec 30 §5 item 3, invariant I9).
                let stop = err.kind == ErrorKind::Request;
                last = Some(err);
                if stop {
                    break;
                }
            }
        }
    }
    Err(last.unwrap_or_else(|| GatewayError::internal("no account attempt succeeded")))
}

/// Streaming proxy request (spec 30 §6, spec 16).
///
/// The returned stream is the client's SSE body; the response status is always
/// 200 because the commit decision has already been made when a stream is
/// returned. Pre-commit failures come back as `Err`.
pub async fn handle_stream(
    services: &Services,
    protocol: ClientProtocol,
    body: &serde_json::Value,
) -> Result<stream::SseStream, GatewayError> {
    let prepared = prepare(services, protocol, body)?;
    if services.accounts.is_empty() {
        return Err(no_account());
    }
    let mut last: Option<GatewayError> = None;
    for name in services.accounts.attempt_order(now_unix()) {
        let Some(account) = services.accounts.snapshot(&name) else {
            continue;
        };
        note_benched(&account);
        let token = match account_token(services, &account, now_unix()).await {
            Ok(token) => token,
            Err(err) => {
                charge_failure(services, &account, &err);
                last = Some(err);
                continue;
            }
        };
        let provider_request = prepared
            .encoder
            .encode_request(&prepared.request, &prepared.capabilities)?;
        let mut empty_retries = 0u32;
        loop {
            let envelope = build_envelope(prepared.provider, &prepared.model, &provider_request)?;
            let request = completions_request(&envelope, &token, true);
            let Ok(upstream) = services.upstream.execute_stream(request).await else {
                let err = as_transient(GatewayError::transient(None, "upstream stream failed"));
                charge_failure(services, &account, &err);
                last = Some(err);
                break;
            };
            if !(200..300).contains(&upstream.status) {
                let body = read_capped(upstream.body).await;
                let failure = classify_attempt(upstream.status, &body, true).unwrap_or_else(|| {
                    AttemptFailure::new(
                        GatewayError::transient(
                            Some(upstream.status),
                            "upstream rejected the stream",
                        ),
                        false,
                        Duration::ZERO,
                    )
                });
                let err = failure.error;
                if err.kind == ErrorKind::Request {
                    return Err(err);
                }
                charge_failure(services, &account, &err);
                last = Some(err);
                break;
            }
            match pre_read(upstream.body, &prepared).await {
                PreRead::Ready { run, pending } => {
                    // The stream has produced a model event: from here on the
                    // client is committed (spec 16 §4).
                    // shortcut: success is written at commit, so a mid-stream
                    // failure or client drop still leaves the account healthy;
                    // upgrade when the stream can own a handle to the store.
                    succeed(services, &account);
                    return Ok(sse_stream(*run, pending));
                }
                PreRead::Empty => {
                    if empty_retries < stream::EMPTY_STREAM_RETRIES {
                        empty_retries += 1;
                        wait(Duration::from_millis(stream::EMPTY_STREAM_BACKOFF_MS)).await;
                        continue;
                    }
                    let err =
                        GatewayError::transient(Some(200), "upstream returned an empty stream");
                    charge_failure(services, &account, &err);
                    last = Some(err);
                    break;
                }
                PreRead::Failed(err) => {
                    if err.kind == ErrorKind::Request {
                        return Err(err);
                    }
                    charge_failure(services, &account, &err);
                    last = Some(err);
                    break;
                }
            }
        }
    }
    Err(last.unwrap_or_else(|| GatewayError::internal("no account attempt succeeded")))
}

// ---------------------------------------------------------------------------
// Request preparation and client-side effort validation (spec 70 §1.2)
// ---------------------------------------------------------------------------

struct Prepared {
    request: NeutralRequest,
    model: String,
    provider: Provider,
    protocol: ClientProtocol,
    capabilities: ModelCapabilities,
    encoder: Box<dyn ProviderEncoder>,
    content_type: &'static str,
}

fn prepare(
    services: &Services,
    protocol: ClientProtocol,
    body: &serde_json::Value,
) -> Result<Prepared, GatewayError> {
    let catalog = services.catalog.snapshot();
    let mut request = crate::client::decoder_for(protocol).decode(body)?;
    let model = request.model.clone();
    // Dispatch is by the live registry, never by client-protocol hints (I4).
    let capabilities = catalog.capabilities(&model)?;
    request.provider = capabilities.provider;
    validate_effort(protocol, &model, &request)?;
    Ok(Prepared {
        provider: capabilities.provider,
        encoder: crate::provider::encoder_for(capabilities.provider),
        protocol,
        content_type: protocol.content_type(),
        request,
        model,
        capabilities,
    })
}

/// GPT-5.6 rejects `max`/`minimal` on the OpenAI routes before any upstream
/// call (spec 70 §1.2, spec 30 §3.5). `/v1/messages` is exempt.
fn validate_effort(
    protocol: ClientProtocol,
    model: &str,
    request: &NeutralRequest,
) -> Result<(), GatewayError> {
    if protocol == ClientProtocol::AnthropicMessages || !model.starts_with("gpt-5.6") {
        return Ok(());
    }
    let Some(effort) = request.reasoning.effort.as_deref() else {
        return Ok(());
    };
    let effort = effort.trim().to_ascii_lowercase();
    if effort.is_empty() || crate::provider::openai::GPT_56_EFFORTS.contains(&effort.as_str()) {
        return Ok(());
    }
    let mut err = GatewayError::request(
        "Zed-hosted GPT-5.6 supports none, low, medium, high, and xhigh; max/minimal are not available on this upstream route",
    );
    err.code = Some("unsupported_reasoning_effort");
    Err(err)
}

fn no_account() -> GatewayError {
    let mut err = GatewayError::request("no account configured");
    err.code = Some("no_account");
    err
}

// ---------------------------------------------------------------------------
// Non-streaming attempt loop (spec 30 §5, spec 50 §7.1)
// ---------------------------------------------------------------------------

async fn run_account(
    services: &Services,
    prepared: &Prepared,
    account: &AccountSnapshot,
) -> Result<ProxyOutcome, GatewayError> {
    let mut token = match account_token(services, account, now_unix()).await {
        Ok(token) => token,
        Err(err) => {
            charge_failure(services, account, &err);
            return Err(err);
        }
    };
    let provider_request = prepared
        .encoder
        .encode_request(&prepared.request, &prepared.capabilities)?;
    let mut attempt = 1u32;
    let mut reminted = false;
    loop {
        let envelope = build_envelope(prepared.provider, &prepared.model, &provider_request)?;
        let request = completions_request(&envelope, &token, false);
        let response = match services.upstream.execute(request).await {
            Ok(response) => response,
            Err(err) => {
                let err = as_transient(err);
                if attempt < MAX_ATTEMPTS {
                    wait(backoff_before(ErrorKind::Transient, attempt + 1)).await;
                    attempt += 1;
                    continue;
                }
                charge_failure(services, account, &err);
                return Err(err);
            }
        };
        if let Some(failure) = classify_attempt(response.status, &response.body, false) {
            // 401: drop the cached token, mint once, retry immediately
            // (spec 30 §3.4). 403 never re-mints (spec 30 §7 E6).
            if failure.error.kind == ErrorKind::Auth
                && failure.error.status == Some(401)
                && !reminted
            {
                reminted = true;
                clear_account_token(services, &account.name);
                match account_token(services, account, now_unix()).await {
                    Ok(fresh) => {
                        token = fresh;
                        continue;
                    }
                    Err(err) => {
                        charge_failure(services, account, &err);
                        return Err(err);
                    }
                }
            }
            if failure.tries_again(attempt) {
                wait(failure.backoff).await;
                attempt += 1;
                continue;
            }
            let err = failure.error;
            charge_failure(services, account, &err);
            return Err(err);
        }
        let reduced = match prepared.encoder.decode_response(&response.body) {
            Ok(reduced) => reduced,
            Err(err) => {
                let err = as_transient(err);
                if attempt < MAX_ATTEMPTS {
                    wait(backoff_before(ErrorKind::Transient, attempt + 1)).await;
                    attempt += 1;
                    continue;
                }
                charge_failure(services, account, &err);
                return Err(err);
            }
        };
        let body = crate::client::encoder_for(prepared.protocol).encode_response(
            &reduced,
            &prepared.model,
            now_unix(),
        )?;
        succeed(services, account);
        return Ok(ProxyOutcome {
            status: 200,
            content_type: prepared.content_type,
            body,
        });
    }
}

pub(crate) fn completions_request(envelope: &[u8], token: &str, stream: bool) -> UpstreamRequest {
    let mut request = UpstreamRequest::new(Endpoint::Completions)
        .bearer(token)
        .json_body(envelope.to_vec())
        .zed_client();
    request.stream = stream;
    request
}

fn succeed(services: &Services, account: &AccountSnapshot) {
    services.accounts.record_success(&account.name, now_unix());
    if services.accounts.current_name() != account.name {
        services.accounts.set_current(&account.name);
        log(&format!("failover success: switched to '{}'", account.name));
    }
}

/// Charge an account-level failure and log it (spec 50 §5.3–§5.4, §7.3).
/// Request-level and plan-level failures never touch the cooldown state.
fn charge_failure(services: &Services, account: &AccountSnapshot, err: &GatewayError) {
    if !err.kind.is_account_level() {
        return;
    }
    let now = now_unix();
    services
        .accounts
        .record_failure(&account.name, err.kind, err.status.unwrap_or(0), now);
    let cooldown = services
        .accounts
        .snapshot(&account.name)
        .map(|snapshot| (snapshot.disabled_until - now).max(0))
        .unwrap_or(0);
    log(&format!(
        "account '{}' failed: {} (kind={}, benched {}s)",
        account.name,
        err.message,
        err.kind.as_str(),
        cooldown
    ));
}

fn note_benched(account: &AccountSnapshot) {
    let now = now_unix();
    if account.disabled_until > now {
        log(&format!(
            "account '{}' is benched (retry in {}s), trying anyway as last resort",
            account.name,
            account.disabled_until - now
        ));
    }
}

fn as_transient(err: GatewayError) -> GatewayError {
    if err.kind == ErrorKind::Transient {
        return err;
    }
    GatewayError::transient(err.status, err.message)
}

async fn wait(backoff: Duration) {
    if backoff > Duration::ZERO {
        tokio::time::sleep(backoff).await;
    }
}

// ---------------------------------------------------------------------------
// Per-account token cache (spec 00 §5, spec 30 §3.4)
//
// shortcut: `AccountStore` exposes no token accessor, so the cache lives here,
// keyed by (data dir, account name); move it into `Account.token` once the
// store grows an accessor.
// ---------------------------------------------------------------------------

type TokenKey = (PathBuf, String);

fn token_pool() -> &'static Mutex<HashMap<TokenKey, TokenCache>> {
    static POOL: OnceLock<Mutex<HashMap<TokenKey, TokenCache>>> = OnceLock::new();
    POOL.get_or_init(|| Mutex::new(HashMap::new()))
}

fn pool_lock() -> std::sync::MutexGuard<'static, HashMap<TokenKey, TokenCache>> {
    token_pool()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn token_key(services: &Services, name: &str) -> TokenKey {
    (services.accounts.data_dir().to_path_buf(), name.to_string())
}

/// Cached LLM token for one account, minted when missing or within 60 s of
/// expiry. Shared with the probe path.
pub(crate) async fn account_token(
    services: &Services,
    account: &AccountSnapshot,
    now: i64,
) -> Result<String, GatewayError> {
    let key = token_key(services, &account.name);
    let mut cache = pool_lock().get(&key).cloned().unwrap_or_default();
    let token = cached_or_minted(
        services.upstream.as_ref(),
        &mut cache,
        &account.user_id,
        &account.credential,
        now,
    )
    .await?;
    pool_lock().insert(key, cache);
    Ok(token)
}

fn clear_account_token(services: &Services, name: &str) {
    if let Some(cache) = pool_lock().get_mut(&token_key(services, name)) {
        cache.clear();
    }
}

// ---------------------------------------------------------------------------
// Streaming shell (spec 16)
// ---------------------------------------------------------------------------

struct StreamRun {
    upstream: ByteStream,
    decoder: Box<dyn StreamDecoder>,
    client: Box<dyn ClientEncoder>,
    state: stream::SseState,
    lines: stream::LineBuffer,
    /// Lines split out of a chunk but not yet fed to the decoder. A network
    /// chunk can hold several upstream lines, and the pre-read stops as soon as
    /// the first model event appears; the rest must survive into the stream.
    queued: std::collections::VecDeque<String>,
    /// At least one data line arrived (even a keep-alive), so this is not the
    /// "empty HTTP 200 stream" case (spec 16 §5).
    saw_line: bool,
    /// The client protocol's closing frames were already written once.
    closed: bool,
}

/// Outcome of draining the already-buffered upstream lines.
enum Pumped {
    /// Every buffered line was consumed; more bytes are needed.
    Progress,
    /// A model event appeared: the client may now be committed.
    Ready,
    /// Structural `{"status":"stream_ended"}` (spec 16 §3).
    Ended,
    Failed(GatewayError),
}

fn pump_lines(run: &mut StreamRun, out: &mut Vec<u8>) -> Pumped {
    while let Some(line) = run.queued.pop_front() {
        // Blank lines are ignored (spec 16 §2 item 3) and do not make the
        // stream non-empty (spec 16 §5).
        if !crate::provider::clean_line(&line).is_empty() {
            run.saw_line = true;
        }
        if stream::line_is_stream_ended(&line) {
            return Pumped::Ended;
        }
        match feed_line(run, &line, out) {
            Ok(None) => {}
            Ok(Some(err)) | Err(err) => return Pumped::Failed(err),
        }
        if run.decoder.saw_model_event() {
            return Pumped::Ready;
        }
    }
    Pumped::Progress
}

enum PreRead {
    /// The upstream produced a model event; the client may now be committed.
    Ready {
        run: Box<StreamRun>,
        pending: Vec<u8>,
    },
    /// HTTP 200 with no data before EOF (spec 16 §5).
    Empty,
    Failed(GatewayError),
}

/// Read the upstream until the first model event so pre-commit failures can
/// still become a non-200 response.
async fn pre_read(upstream: ByteStream, prepared: &Prepared) -> PreRead {
    let mut state = stream::SseState::new(prepared.model.clone(), now_unix());
    state.include_usage = prepared.request.include_usage;
    let mut run = StreamRun {
        upstream,
        decoder: prepared.encoder.stream_decoder(),
        client: crate::client::encoder_for(prepared.protocol),
        state,
        lines: stream::LineBuffer::default(),
        queued: std::collections::VecDeque::new(),
        saw_line: false,
        closed: false,
    };
    let mut pending = Vec::new();
    loop {
        match pump_lines(&mut run, &mut pending) {
            Pumped::Progress => {}
            Pumped::Ready => {
                return PreRead::Ready {
                    run: Box::new(run),
                    pending,
                }
            }
            Pumped::Ended => {
                let empty = !run.saw_line && !run.decoder.saw_model_event();
                if empty {
                    return PreRead::Empty;
                }
                close_normal(&mut run, &mut pending);
                return PreRead::Ready {
                    run: Box::new(run),
                    pending,
                };
            }
            Pumped::Failed(err) => return PreRead::Failed(err),
        }
        match run.upstream.next().await {
            None => {
                if let Some(line) = run.lines.finish() {
                    run.queued.push_back(line);
                }
                if let Pumped::Failed(err) = pump_lines(&mut run, &mut pending) {
                    return PreRead::Failed(err);
                }
                if !run.saw_line && !run.decoder.saw_model_event() {
                    return PreRead::Empty;
                }
                close_normal(&mut run, &mut pending);
                return PreRead::Ready {
                    run: Box::new(run),
                    pending,
                };
            }
            Some(Err(err)) => return PreRead::Failed(as_transient(err)),
            Some(Ok(chunk)) => {
                run.queued.extend(run.lines.push(&chunk));
            }
        }
    }
}

/// Feed one upstream line; `Ok(Some(err))` is a terminal upstream failure. The
/// decoder's `Error` event is deliberately not encoded here: the caller picks
/// the pre-commit (propagate) or post-commit (one error frame) treatment.
fn feed_line(
    run: &mut StreamRun,
    line: &str,
    out: &mut Vec<u8>,
) -> Result<Option<GatewayError>, GatewayError> {
    let events = run.decoder.push_line(line)?;
    if let Some(err) = run.decoder.failure() {
        return Ok(Some(err));
    }
    encode_events(run.client.as_ref(), &mut run.state, &events, out)?;
    Ok(None)
}

fn encode_events(
    client: &dyn ClientEncoder,
    state: &mut stream::SseState,
    events: &[StreamEvent],
    out: &mut Vec<u8>,
) -> Result<(), GatewayError> {
    for event in events {
        if let Some(frame) = client.encode_event(event, state)? {
            out.extend(frame);
        }
    }
    Ok(())
}

/// Normal end of the upstream stream: decoder terminal events plus the client
/// protocol's own closing frames (spec 16 §8). Called at most once.
fn close_normal(run: &mut StreamRun, out: &mut Vec<u8>) {
    if run.closed {
        return;
    }
    match run.decoder.finish() {
        Ok(events) => {
            if let Err(err) = encode_events(run.client.as_ref(), &mut run.state, &events, out) {
                emit_failure(run, &err, out);
                return;
            }
        }
        Err(err) => {
            emit_failure(run, &err, out);
            return;
        }
    }
    if let Some(err) = run.decoder.failure() {
        emit_failure(run, &err, out);
        return;
    }
    run.closed = true;
    if let Ok(tail) = run.client.as_ref().finish(&mut run.state) {
        out.extend(tail);
    }
}

/// Post-commit failure: one protocol error frame, then the closing frames
/// (spec 16 §4, spec 30 §6). Chat still needs its `[DONE]` after the error,
/// which `finish` supplies while the state is terminal.
fn emit_failure(run: &mut StreamRun, err: &GatewayError, out: &mut Vec<u8>) {
    if run.closed {
        return;
    }
    run.closed = true;
    let _ = encode_events(
        run.client.as_ref(),
        &mut run.state,
        &[StreamEvent::Error(err.clone())],
        out,
    );
    if let Ok(tail) = run.client.as_ref().finish(&mut run.state) {
        out.extend(tail);
    }
}

struct StreamStep {
    run: StreamRun,
    pending: Vec<u8>,
    finished: bool,
}

fn sse_stream(run: StreamRun, pending: Vec<u8>) -> stream::SseStream {
    let initial = StreamStep {
        run,
        pending,
        finished: false,
    };
    Box::pin(futures_util::stream::unfold(
        initial,
        |mut step| async move {
            if step.finished {
                return None;
            }
            if !step.pending.is_empty() {
                let bytes = Bytes::from(std::mem::take(&mut step.pending));
                return Some((Ok(bytes), step));
            }
            loop {
                // Lines left over from the pre-read (and from earlier chunks)
                // go first; only then ask the upstream for more bytes.
                let mut out = Vec::new();
                match pump_lines(&mut step.run, &mut out) {
                    Pumped::Progress => {
                        if !out.is_empty() {
                            return Some((Ok(Bytes::from(out)), step));
                        }
                    }
                    Pumped::Ready => {
                        if !out.is_empty() {
                            return Some((Ok(Bytes::from(out)), step));
                        }
                        continue;
                    }
                    Pumped::Ended => {
                        close_normal(&mut step.run, &mut out);
                        step.finished = true;
                        if out.is_empty() {
                            return None;
                        }
                        return Some((Ok(Bytes::from(out)), step));
                    }
                    Pumped::Failed(err) => {
                        emit_failure(&mut step.run, &err, &mut out);
                        step.finished = true;
                        return Some((Ok(Bytes::from(out)), step));
                    }
                }
                match step.run.upstream.next().await {
                    None => {
                        let mut out = Vec::new();
                        if let Some(line) = step.run.lines.finish() {
                            step.run.queued.push_back(line);
                        }
                        if let Pumped::Failed(err) = pump_lines(&mut step.run, &mut out) {
                            emit_failure(&mut step.run, &err, &mut out);
                            step.finished = true;
                            return Some((Ok(Bytes::from(out)), step));
                        }
                        close_normal(&mut step.run, &mut out);
                        step.finished = true;
                        if out.is_empty() {
                            return None;
                        }
                        return Some((Ok(Bytes::from(out)), step));
                    }
                    Some(Err(err)) => {
                        let mut out = Vec::new();
                        emit_failure(&mut step.run, &as_transient(err), &mut out);
                        step.finished = true;
                        return Some((Ok(Bytes::from(out)), step));
                    }
                    Some(Ok(chunk)) => {
                        step.run.queued.extend(step.run.lines.push(&chunk));
                    }
                }
            }
        },
    ))
}

async fn read_capped(mut body: ByteStream) -> Vec<u8> {
    let mut collected = Vec::new();
    while let Some(chunk) = body.next().await {
        match chunk {
            Ok(bytes) => {
                collected.extend_from_slice(&bytes);
                if collected.len() > 1024 * 1024 {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    collected
}

// ---------------------------------------------------------------------------
// Small shared helpers
// ---------------------------------------------------------------------------

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

fn log(message: &str) {
    eprintln!("[zed-proxy] {message}");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::upstream::client::{BoxFuture, UpstreamResponse, UpstreamStream};
    use serde_json::Value;
    use std::collections::VecDeque;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex as StdMutex};

    type Reply = (u16, Vec<u8>);

    /// Fake upstream: queued replies per endpoint, plus every request body.
    /// `Arc`-shared so a test can inspect the counters after boxing it.
    #[derive(Default)]
    struct FakeUpstream {
        completions: StdMutex<VecDeque<Reply>>,
        me: StdMutex<VecDeque<Reply>>,
        tokens: StdMutex<VecDeque<Reply>>,
        bodies: StdMutex<Vec<Vec<u8>>>,
        streams: StdMutex<VecDeque<(u16, Vec<Vec<u8>>)>>,
        calls: AtomicUsize,
    }

    impl FakeUpstream {
        fn new() -> Arc<Self> {
            Arc::new(Self::default())
        }

        fn push_completion(&self, status: u16, body: impl Into<Vec<u8>>) {
            self.completions
                .lock()
                .unwrap()
                .push_back((status, body.into()));
        }

        fn push_stream(&self, status: u16, chunks: Vec<&str>) {
            self.streams.lock().unwrap().push_back((
                status,
                chunks
                    .into_iter()
                    .map(|chunk| chunk.as_bytes().to_vec())
                    .collect(),
            ));
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }

        fn bodies(&self) -> Vec<String> {
            self.bodies
                .lock()
                .unwrap()
                .iter()
                .map(|body| String::from_utf8_lossy(body).into_owned())
                .collect()
        }
    }

    fn meta(
        queue: &StdMutex<VecDeque<Reply>>,
        fallback: &[u8],
    ) -> Result<UpstreamResponse, GatewayError> {
        let (status, body) = queue
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or((200, fallback.to_vec()));
        Ok(UpstreamResponse { status, body })
    }

    impl UpstreamClient for FakeUpstream {
        fn execute<'a>(
            &'a self,
            request: UpstreamRequest,
        ) -> BoxFuture<'a, Result<UpstreamResponse, GatewayError>> {
            self.bodies.lock().unwrap().push(request.body.clone());
            match request.endpoint {
                Endpoint::UserMe | Endpoint::Models => {
                    Box::pin(
                        async move { meta(&self.me, br#"{"default_organization_id":"org-1"}"#) },
                    )
                }
                Endpoint::LlmTokens => Box::pin(async move {
                    let fallback = format!(r#"{{"token":"{}"}}"#, jwt(4102444800));
                    meta(&self.tokens, fallback.as_bytes())
                }),
                Endpoint::Completions => {
                    self.calls.fetch_add(1, Ordering::SeqCst);
                    let next = self.completions.lock().unwrap().pop_front();
                    Box::pin(async move {
                        let (status, body) =
                            next.ok_or_else(|| GatewayError::internal("no queued reply"))?;
                        Ok(UpstreamResponse { status, body })
                    })
                }
            }
        }

        fn execute_stream<'a>(
            &'a self,
            request: UpstreamRequest,
        ) -> BoxFuture<'a, Result<UpstreamStream, GatewayError>> {
            self.bodies.lock().unwrap().push(request.body.clone());
            self.calls.fetch_add(1, Ordering::SeqCst);
            let next = self.streams.lock().unwrap().pop_front();
            Box::pin(async move {
                let Some((status, chunks)) = next else {
                    return Err(GatewayError::transient(None, "no queued stream"));
                };
                let items: Vec<Result<Bytes, GatewayError>> = chunks
                    .into_iter()
                    .map(|chunk| Ok(Bytes::from(chunk)))
                    .collect();
                Ok(UpstreamStream {
                    status,
                    body: Box::pin(futures_util::stream::iter(items)),
                })
            })
        }
    }

    fn jwt(exp: i64) -> String {
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        use base64::Engine as _;
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"none"}"#);
        let payload = URL_SAFE_NO_PAD.encode(format!(r#"{{"exp":{exp}}}"#).as_bytes());
        format!("{header}.{payload}.sig")
    }

    /// One directory under the system temp root, removed on drop (no external
    /// crate is available for this).
    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new() -> Self {
            let unique = format!(
                "zprs-proxy-test-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|value| value.as_nanos())
                    .unwrap_or(0)
            );
            let path = std::env::temp_dir().join(unique);
            std::fs::create_dir_all(&path).unwrap();
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    fn services(upstream: &Arc<FakeUpstream>, accounts: &[&str]) -> (Services, TempDir) {
        let dir = TempDir::new();
        let names: Vec<String> = accounts
            .iter()
            .enumerate()
            .map(|(index, name)| {
                format!("\"{name}\":{{\"user_id\":\"{index}\",\"credential\":{{\"k\":\"v\"}}}}")
            })
            .collect();
        std::fs::write(
            dir.path().join("accounts.json"),
            format!("{{\"accounts\":{{{}}}}}", names.join(",")),
        )
        .unwrap();
        let store = AccountStore::load(dir.path(), 100);
        let services = Services {
            catalog: CatalogHandle::new(crate::catalog::Catalog::builtin().unwrap()),
            accounts: store,
            upstream: Box::new(FakeUpstreamHandle(Arc::clone(upstream))),
        };
        (services, dir)
    }

    /// Delegating wrapper so `Services` owns the trait object while the test
    /// keeps the same fake for assertions.
    struct FakeUpstreamHandle(Arc<FakeUpstream>);

    impl UpstreamClient for FakeUpstreamHandle {
        fn execute<'a>(
            &'a self,
            request: UpstreamRequest,
        ) -> BoxFuture<'a, Result<UpstreamResponse, GatewayError>> {
            self.0.execute(request)
        }

        fn execute_stream<'a>(
            &'a self,
            request: UpstreamRequest,
        ) -> BoxFuture<'a, Result<UpstreamStream, GatewayError>> {
            self.0.execute_stream(request)
        }
    }

    fn chat_body(model: &str) -> Value {
        serde_json::json!({
            "model": model,
            "messages": [{"role": "user", "content": "hi"}],
            "stream": false
        })
    }

    fn stream_body(model: &str) -> Value {
        serde_json::json!({
            "model": model,
            "messages": [{"role": "user", "content": "hi"}],
            "stream": true
        })
    }

    const OK_RESPONSE: &str = "{\"type\":\"response.created\",\"response\":{\"id\":\"r\"}}\n{\"type\":\"response.output_text.delta\",\"delta\":\"OK\"}\n{\"type\":\"response.completed\",\"response\":{\"id\":\"r\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n";

    async fn drain(mut stream: stream::SseStream) -> String {
        use futures_util::StreamExt as _;
        let mut out = Vec::new();
        while let Some(chunk) = stream.next().await {
            out.extend(chunk.unwrap());
        }
        String::from_utf8(out).unwrap()
    }

    #[tokio::test]
    async fn non_streaming_success_returns_the_client_body() {
        let upstream = FakeUpstream::new();
        upstream.push_completion(200, OK_RESPONSE);
        let (services, _dir) = services(&upstream, &["a"]);
        let outcome = handle_non_stream(
            &services,
            ClientProtocol::ChatCompletions,
            &chat_body("gpt-5.6-luna"),
        )
        .await
        .unwrap();
        assert_eq!(outcome.status, 200);
        let text = String::from_utf8(outcome.body).unwrap();
        assert!(text.contains("\"content\":\"OK\""), "{text}");
        assert_eq!(upstream.calls(), 1);
        assert_eq!(services.accounts.snapshots()[0].consecutive_failures, 0);
    }

    #[tokio::test]
    async fn request_level_400_is_returned_without_retry_cooldown_or_failover() {
        let upstream = FakeUpstream::new();
        upstream.push_completion(400, r#"{"error":{"message":"bad schema"}}"#);
        let (services, _dir) = services(&upstream, &["a", "b"]);
        let err = handle_non_stream(
            &services,
            ClientProtocol::ChatCompletions,
            &chat_body("gpt-5.6-luna"),
        )
        .await
        .unwrap_err();
        assert_eq!(err.kind, ErrorKind::Request);
        assert_eq!(err.message, "bad schema");
        // A single completions call: no same-account retry and no failover.
        assert_eq!(upstream.calls(), 1);
        for snapshot in services.accounts.snapshots() {
            assert_eq!(snapshot.consecutive_failures, 0, "{}", snapshot.name);
            assert_eq!(snapshot.disabled_until, 0, "{}", snapshot.name);
            assert_eq!(snapshot.last_status, 0, "{}", snapshot.name);
        }
    }

    #[tokio::test]
    async fn request_level_404_405_413_415_422_are_also_not_retried() {
        for status in [404u16, 405, 413, 415, 422] {
            let upstream = FakeUpstream::new();
            upstream.push_completion(status, format!("{{\"message\":\"rejected {status}\"}}"));
            let (services, _dir) = services(&upstream, &["a", "b"]);
            let err = handle_non_stream(
                &services,
                ClientProtocol::ChatCompletions,
                &chat_body("gpt-5.6-luna"),
            )
            .await
            .unwrap_err();
            assert_eq!(err.kind, ErrorKind::Request, "status {status}");
            assert_eq!(err.http_status(), 400, "status {status}");
            assert_eq!(err.message, format!("rejected {status}"));
            assert_eq!(upstream.calls(), 1, "status {status}");
            for snapshot in services.accounts.snapshots() {
                assert_eq!(snapshot.disabled_until, 0, "status {status}");
            }
        }
    }

    #[tokio::test]
    async fn auth_failures_bench_the_account_and_fail_over() {
        let upstream = FakeUpstream::new();
        // The first 401 triggers the one allowed re-mint; the retry's 401 is
        // final, so the account is benched and the next account is tried.
        upstream.push_completion(401, r#"{"message":"token expired"}"#);
        upstream.push_completion(401, r#"{"message":"still expired"}"#);
        upstream.push_completion(200, OK_RESPONSE);
        let (services, _dir) = services(&upstream, &["a", "b"]);
        services.accounts.switch("a").unwrap();
        let outcome = handle_non_stream(
            &services,
            ClientProtocol::ChatCompletions,
            &chat_body("gpt-5.6-luna"),
        )
        .await
        .unwrap();
        assert_eq!(outcome.status, 200);
        let first = services.accounts.snapshot("a").unwrap();
        assert_eq!(first.consecutive_failures, 1);
        assert!(first.disabled_until > 100, "a must be benched");
        assert_eq!(services.accounts.current_name(), "b");
    }

    #[tokio::test]
    async fn all_accounts_transient_failures_report_502() {
        let upstream = FakeUpstream::new();
        for _ in 0..6 {
            upstream.push_completion(500, b"boom".to_vec());
        }
        let (services, _dir) = services(&upstream, &["a"]);
        let err = handle_non_stream(
            &services,
            ClientProtocol::ChatCompletions,
            &chat_body("gpt-5.6-luna"),
        )
        .await
        .unwrap_err();
        assert_eq!(err.kind, ErrorKind::Transient);
        assert_eq!(err.http_status(), 502);
        // 5xx retries the same account up to three attempts (spec 10 §7).
        assert_eq!(upstream.calls(), MAX_ATTEMPTS as usize);
    }

    #[tokio::test]
    async fn streaming_emits_text_and_done_frames() {
        let upstream = FakeUpstream::new();
        upstream.push_stream(
            200,
            vec![
                "{\"type\":\"response.created\",\"response\":{\"id\":\"r\"}}\n",
                "{\"type\":\"response.output_text.delta\",\"delta\":\"OK\"}\n",
                "{\"type\":\"response.completed\",\"response\":{\"id\":\"r\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n",
                "{\"status\":\"stream_ended\"}\n",
            ],
        );
        let (services, _dir) = services(&upstream, &["a"]);
        let stream = handle_stream(
            &services,
            ClientProtocol::ChatCompletions,
            &stream_body("gpt-5.6-luna"),
        )
        .await
        .unwrap();
        let text = drain(stream).await;
        assert!(text.contains("\"content\":\"OK\""), "{text}");
        assert!(text.ends_with("data: [DONE]\n\n"), "{text}");
    }

    #[tokio::test]
    async fn several_upstream_lines_in_one_chunk_all_reach_the_client() {
        let upstream = FakeUpstream::new();
        // One network chunk: model event, completion and the end marker. The
        // pre-read must not drop the lines it did not consume (spec 16 §2-§3).
        upstream.push_stream(
            200,
            vec![
                "{\"type\":\"response.created\",\"response\":{\"id\":\"r\"}}\n{\"type\":\"response.output_text.delta\",\"delta\":\"OK\"}\n{\"type\":\"response.completed\",\"response\":{\"id\":\"r\"}}\n{\"status\":\"stream_ended\"}\n",
            ],
        );
        let (services, _dir) = services(&upstream, &["a"]);
        let stream = handle_stream(
            &services,
            ClientProtocol::ChatCompletions,
            &stream_body("gpt-5.6-luna"),
        )
        .await
        .unwrap();
        let text = drain(stream).await;
        assert!(text.contains("\"content\":\"OK\""), "{text}");
        assert!(text.contains("\"finish_reason\":\"stop\""), "{text}");
        assert!(text.ends_with("data: [DONE]\n\n"), "{text}");
        assert_eq!(upstream.calls(), 1);
    }

    #[tokio::test]
    async fn pre_commit_400_on_a_stream_returns_400_and_does_not_cool() {
        let upstream = FakeUpstream::new();
        upstream.push_stream(
            200,
            vec![
                "{\"status\":{\"failed\":{\"code\":\"upstream_http_400\",\"message\":\"bad tools\",\"retry_after\":null}}}\n",
            ],
        );
        let (services, _dir) = services(&upstream, &["a", "b"]);
        let err = handle_stream(
            &services,
            ClientProtocol::ChatCompletions,
            &stream_body("gpt-5.6-luna"),
        )
        .await
        .err()
        .expect("expected a pre-commit error");
        assert_eq!(err.kind, ErrorKind::Request);
        assert_eq!(err.message, "bad tools");
        assert_eq!(upstream.calls(), 1);
        for snapshot in services.accounts.snapshots() {
            assert_eq!(snapshot.disabled_until, 0);
        }
    }

    #[tokio::test]
    async fn empty_200_streams_retry_twice_on_the_same_account() {
        let upstream = FakeUpstream::new();
        for _ in 0..stream::EMPTY_STREAM_RETRIES {
            upstream.push_stream(200, vec![]);
        }
        upstream.push_stream(
            200,
            vec!["{\"type\":\"response.output_text.delta\",\"delta\":\"OK\"}\n{\"type\":\"response.completed\",\"response\":{\"id\":\"r\"}}\n"],
        );
        let (services, _dir) = services(&upstream, &["a"]);
        let stream = handle_stream(
            &services,
            ClientProtocol::ChatCompletions,
            &stream_body("gpt-5.6-luna"),
        )
        .await
        .unwrap();
        let text = drain(stream).await;
        assert!(text.contains("\"content\":\"OK\""), "{text}");
        assert_eq!(upstream.calls(), 1 + stream::EMPTY_STREAM_RETRIES as usize);
    }

    #[tokio::test]
    async fn exhausted_empty_streams_cool_the_account_and_fail_over() {
        let upstream = FakeUpstream::new();
        for _ in 0..=stream::EMPTY_STREAM_RETRIES {
            upstream.push_stream(200, vec![]);
        }
        upstream.push_stream(
            200,
            vec!["{\"type\":\"response.output_text.delta\",\"delta\":\"OK\"}\n{\"type\":\"response.completed\",\"response\":{\"id\":\"r\"}}\n"],
        );
        let (services, _dir) = services(&upstream, &["a", "b"]);
        services.accounts.switch("a").unwrap();
        let stream = handle_stream(
            &services,
            ClientProtocol::ChatCompletions,
            &stream_body("gpt-5.6-luna"),
        )
        .await
        .unwrap();
        let text = drain(stream).await;
        assert!(text.contains("\"content\":\"OK\""), "{text}");
        let first = services.accounts.snapshot("a").unwrap();
        assert_eq!(first.consecutive_failures, 1);
        assert_eq!(services.accounts.current_name(), "b");
    }

    #[tokio::test]
    async fn a_failure_after_the_first_model_event_is_emitted_in_stream() {
        let upstream = FakeUpstream::new();
        upstream.push_stream(
            200,
            vec![
                "{\"type\":\"response.output_text.delta\",\"delta\":\"partial\"}\n",
                "{\"status\":{\"failed\":{\"code\":\"upstream_http_500\",\"message\":\"upstream broke\",\"retry_after\":null}}}\n",
            ],
        );
        let (services, _dir) = services(&upstream, &["a", "b"]);
        let stream = handle_stream(
            &services,
            ClientProtocol::ChatCompletions,
            &stream_body("gpt-5.6-luna"),
        )
        .await
        .unwrap();
        let text = drain(stream).await;
        assert!(text.contains("partial"), "{text}");
        assert!(text.contains("upstream broke"), "{text}");
        assert!(text.ends_with("data: [DONE]\n\n"), "{text}");
        // Committed: no failover.
        assert_eq!(upstream.calls(), 1);
        assert_eq!(services.accounts.current_name(), "a");
    }

    #[tokio::test]
    async fn anthropic_streams_close_with_message_stop() {
        let upstream = FakeUpstream::new();
        upstream.push_stream(
            200,
            vec![
                "{\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"usage\":{\"input_tokens\":2}}}\n",
                "{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n",
                "{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"OK\"}}\n",
                "{\"type\":\"content_block_stop\",\"index\":0}\n",
                "{\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n",
                "{\"type\":\"message_stop\"}\n",
            ],
        );
        let (services, _dir) = services(&upstream, &["a"]);
        let stream = handle_stream(
            &services,
            ClientProtocol::AnthropicMessages,
            &stream_body("claude-sonnet-5"),
        )
        .await
        .unwrap();
        let text = drain(stream).await;
        assert!(text.contains("message_start"), "{text}");
        assert!(text.contains("\"text\":\"OK\""), "{text}");
        assert!(text.contains("message_stop"), "{text}");
    }

    #[tokio::test]
    async fn google_non_streaming_round_trips() {
        let upstream = FakeUpstream::new();
        upstream.push_completion(
            200,
            "{\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"OK\"}]},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":3,\"candidatesTokenCount\":1,\"totalTokenCount\":4}}\n",
        );
        let (services, _dir) = services(&upstream, &["a"]);
        let outcome = handle_non_stream(
            &services,
            ClientProtocol::ChatCompletions,
            &chat_body("gemini-4-argon"),
        )
        .await
        .unwrap();
        let text = String::from_utf8(outcome.body).unwrap();
        assert!(text.contains("\"content\":\"OK\""), "{text}");
        // Envelope provider is the catalog's, never the client protocol's.
        let bodies = upstream.bodies();
        assert!(
            bodies
                .iter()
                .any(|body| body.contains("\"provider\":\"google\"")),
            "{bodies:?}"
        );
    }

    #[tokio::test]
    async fn no_accounts_is_a_400_per_protocol() {
        let upstream = FakeUpstream::new();
        let (services, _dir) = services(&upstream, &[]);
        let err = handle_non_stream(
            &services,
            ClientProtocol::AnthropicMessages,
            &chat_body("claude-sonnet-5"),
        )
        .await
        .unwrap_err();
        assert_eq!(err.kind, ErrorKind::Request);
        assert_eq!(err.http_status(), 400);
        assert_eq!(upstream.calls(), 0);
        let body = ClientProtocol::AnthropicMessages.error_body(&err);
        assert_eq!(
            String::from_utf8(body).unwrap(),
            "{\"type\":\"error\",\"error\":{\"type\":\"invalid_request_error\",\"message\":\"no account configured\"}}"
        );
    }

    #[tokio::test]
    async fn a_401_remints_the_token_once_before_failing_over() {
        let upstream = FakeUpstream::new();
        upstream.push_completion(401, r#"{"message":"expired"}"#);
        upstream.push_completion(200, OK_RESPONSE);
        let (services, _dir) = services(&upstream, &["a"]);
        let outcome = handle_non_stream(
            &services,
            ClientProtocol::ChatCompletions,
            &chat_body("gpt-5.6-luna"),
        )
        .await
        .unwrap();
        assert_eq!(outcome.status, 200);
        // Two completions calls on the same account: the second used a fresh
        // token, so the token cache was cleared and re-minted.
        assert_eq!(upstream.calls(), 2);
        assert_eq!(services.accounts.snapshots()[0].consecutive_failures, 0);
    }

    #[test]
    fn effort_validation_only_blocks_gpt_56_openai_routes() {
        let mut request = NeutralRequest::default();
        request.reasoning.effort = Some("max".to_string());
        let err =
            validate_effort(ClientProtocol::ChatCompletions, "gpt-5.6-sol", &request).unwrap_err();
        assert_eq!(err.kind, ErrorKind::Request);
        assert_eq!(err.code, Some("unsupported_reasoning_effort"));
        assert!(
            validate_effort(ClientProtocol::AnthropicMessages, "gpt-5.6-sol", &request).is_ok()
        );
        assert!(validate_effort(ClientProtocol::ChatCompletions, "gpt-5.5", &request).is_ok());
        request.reasoning.effort = Some("HIGH".to_string());
        assert!(validate_effort(ClientProtocol::ChatCompletions, "gpt-5.6-luna", &request).is_ok());
        request.reasoning.effort = None;
        assert!(validate_effort(ClientProtocol::ChatCompletions, "gpt-5.6-luna", &request).is_ok());
    }

    #[test]
    fn no_account_error_is_a_request_level_failure() {
        let err = no_account();
        assert_eq!(err.kind, ErrorKind::Request);
        assert_eq!(err.message, "no account configured");
        assert_eq!(err.http_status(), 400);
    }
}
