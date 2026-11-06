//! Route table (spec `70-http-endpoints.md` §2) and the endpoint handlers.
//!
//! Table order is normative: first match wins, there is no 405, and `OPTIONS`
//! is answered after every concrete route so `/`, `/api/event_logging/batch` and
//! the `count_tokens` prefix keep their non-empty OPTIONS bodies (spec 70 §4).

use std::sync::Arc;

use axum::extract::{DefaultBodyLimit, State};
use axum::http::{header, HeaderValue, Method, Request, StatusCode};
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::routing::any;
use axum::Router;
use serde_json::Value;

use crate::client::ClientProtocol;
use crate::error::GatewayError;
use crate::proxy::Services;

use super::body;
use super::static_file;

/// Shared state handed to every handler.
#[derive(Clone)]
pub struct AppState {
    pub services: Arc<Services>,
    pub keys: Arc<super::auth::GatewayKeys>,
}

/// Build the full application router.
pub fn build_router(state: AppState) -> Router {
    let routes = Router::new()
        .route("/", any(root_handler))
        .route("/health", any(health_handler))
        .route("/v1/models", any(models_handler))
        .route("/api/event_logging/batch", any(event_logging_handler))
        .route("/v1/chat/completions", any(chat_completions_handler))
        .route("/v1/responses", any(responses_handler))
        .route("/v1/messages", any(messages_handler))
        .route("/zed/accounts", any(accounts_handler))
        .route("/zed/accounts/status", any(accounts_status_handler))
        .route("/zed/accounts/health", any(accounts_health_handler))
        .route("/zed/accounts/orb", any(accounts_orb_handler))
        .route(
            "/zed/accounts/orb/refresh",
            any(accounts_orb_refresh_handler),
        )
        .route("/zed/accounts/switch", any(accounts_switch_handler))
        .route("/zed/models/refresh", any(models_refresh_handler))
        .route("/zed/usage", any(usage_handler))
        .route("/zed/billing", any(billing_handler))
        .route("/zed/login", any(login_start_handler))
        .route("/zed/login/complete", any(login_complete_handler))
        .route("/zed/login/status", any(login_status_handler))
        // `count_tokens` is a prefix route: Claude Code requests suffixed
        // variants (spec 70 §2.1).
        .fallback(any(fallback_handler));

    // `Bytes` extractors would reject at axum's default 2 MiB; the server
    // already reads only the declared `Content-Length` (capped at 16 MiB), so
    // this raises the extractor cap to the same value without unbounded reads.
    routes
        .layer(DefaultBodyLimit::max(
            super::body::MAX_REQUEST_BODY as usize,
        ))
        .layer(middleware::from_fn_with_state(state.clone(), entry_guard))
        .with_state(state)
}

/// Entry guard: the two prefixed rejections that happen before routing
/// (spec 70 §1.1, spec 55 §3).
async fn entry_guard(
    State(state): State<AppState>,
    request: Request<axum::body::Body>,
    next: Next,
) -> Response {
    let method = request.method().as_str().to_string();
    let path = request.uri().path().to_string();

    if super::auth::requires_auth(&state.keys, &method, &path) {
        let headers = request
            .headers()
            .iter()
            .map(|(name, value)| {
                (
                    name.as_str().to_string(),
                    value.to_str().unwrap_or_default().to_string(),
                )
            })
            .collect::<Vec<_>>();
        let presented = super::auth::presented_credential(&headers);
        let allowed = presented
            .as_deref()
            .map(|credential| state.keys.verify(credential))
            .unwrap_or(false);
        if !allowed {
            return with_common_headers(body::json_response(
                StatusCode::UNAUTHORIZED,
                super::auth::UNAUTHORIZED_BODY.as_bytes().to_vec(),
            ));
        }
    }

    if method != "OPTIONS" && body::exceeds_limit(request.headers()) {
        return with_common_headers(body::json_response(
            StatusCode::PAYLOAD_TOO_LARGE,
            body::BODY_TOO_LARGE.as_bytes().to_vec(),
        ));
    }

    // OPTIONS is answered here so a concrete route's method check can never
    // turn it into a 404 (spec 70 §1, §4). The three routes whose OPTIONS
    // answer is non-empty fall through to their handlers.
    if method == "OPTIONS" && !options_keeps_body(&path) {
        return with_common_headers(empty_ok());
    }

    with_common_headers(next.run(request).await)
}

/// Routes that answer `OPTIONS` with their normal (non-empty) body.
fn options_keeps_body(path: &str) -> bool {
    path == "/" || path == "/api/event_logging/batch" || path.starts_with(COUNT_TOKENS_PREFIX)
}

/// Apply the CORS headers every response carries (spec 70 §3).
pub fn with_common_headers(mut response: Response) -> Response {
    let headers = response.headers_mut();
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static("*"),
    );
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("GET, POST, OPTIONS"),
    );
    headers.insert(header::CONNECTION, HeaderValue::from_static("close"));
    response
}

fn not_found() -> Response {
    body::json_response(StatusCode::NOT_FOUND, body::NOT_FOUND.as_bytes().to_vec())
}

fn empty_ok() -> Response {
    body::json_response(StatusCode::OK, Vec::new())
}

/// Only this exact path is the count_tokens prefix route; it is checked before
/// the OPTIONS fallback so its OPTIONS answer stays non-empty (spec 70 §4).
const COUNT_TOKENS_PREFIX: &str = "/v1/messages/count_tokens";

// ---------------------------------------------------------------------------
// Handlers. Each is one endpoint of spec 70 §2.
// ---------------------------------------------------------------------------

/// `GET /` — embedded management page, any method (spec 70 §3.1).
pub async fn root_handler(_request: Request<axum::body::Body>) -> Response {
    let mut response = body::text_response(
        StatusCode::OK,
        static_file::CONTENT_TYPE,
        static_file::INDEX_HTML.to_vec(),
    );
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(static_file::CACHE_CONTROL),
    );
    response
}

/// `GET /health` — public, no version or account data (spec 70 §3.2).
pub async fn health_handler(method: Method) -> Response {
    if method != Method::GET {
        return not_found();
    }
    body::json_response(StatusCode::OK, br#"{"status":"ok"}"#.to_vec())
}

/// `GET /v1/models` — OpenAI list + Codex list (spec 70 §3.3, spec 15 §6).
/// Always 200: upstream failures fall back to the old cache or the built-in
/// catalog (spec 15 §4, §6.3).
pub async fn models_handler(State(state): State<AppState>, method: Method) -> Response {
    if method != Method::GET {
        return not_found();
    }
    let body = models_payload(&state, super::server::now_unix()).await;
    body::json_response(StatusCode::OK, body)
}

/// Cached `/v1/models` bytes, refreshing passively when the hour is up
/// (spec 15 §4). Without any account the built-in catalog is returned and
/// nothing is cached.
async fn models_payload(state: &AppState, now: i64) -> Vec<u8> {
    let services = &state.services;
    if let Some(cached) = services.catalog.cached_models(now) {
        return cached;
    }
    let fallback = |services: &Services| {
        services
            .catalog
            .stale_models()
            .unwrap_or_else(|| services.catalog.rebuild_models(now))
    };
    let Some(account) = first_account(services) else {
        return fallback(services);
    };
    // One pull at a time; a concurrent request serves the old value rather
    // than stacking upstream calls (spec 15 §4).
    let Some(_guard) = services.catalog.try_lock_pull() else {
        return fallback(services);
    };
    // A real pull always reserves the 5 s window, success or failure
    // (spec 15 §5); if a refresh just ran, its output is the cache.
    if services.catalog.reserve_refresh(now) > 0 {
        return fallback(services);
    }
    match refresh_catalog(services, &account).await {
        Ok(()) => services.catalog.rebuild_models(now),
        Err(_) => fallback(services),
    }
}

fn first_account(services: &Services) -> Option<crate::accounts::AccountSnapshot> {
    let current = services.accounts.current_name();
    if !current.is_empty() {
        if let Some(account) = services.accounts.snapshot(&current) {
            return Some(account);
        }
    }
    services.accounts.snapshots().into_iter().next()
}

/// Pull `/models` and replace the registry only on success (spec 15 §2.2).
async fn refresh_catalog(
    services: &Services,
    account: &crate::accounts::AccountSnapshot,
) -> Result<(), GatewayError> {
    let catalog = crate::upstream::fetch_models(
        services.upstream.as_ref(),
        &account.user_id,
        &account.credential,
    )
    .await?;
    services.catalog.replace(catalog);
    services.catalog.invalidate_models();
    Ok(())
}

/// `/api/event_logging/batch` — any method, no body read (spec 70 §3.4).
pub async fn event_logging_handler() -> Response {
    body::json_response(StatusCode::OK, br#"{"status":"ok"}"#.to_vec())
}

/// `POST /v1/chat/completions` (spec 70 §3.15).
pub async fn chat_completions_handler(
    State(state): State<AppState>,
    method: Method,
    bytes: axum::body::Bytes,
) -> Response {
    proxy_entry(state, ClientProtocol::ChatCompletions, method, bytes).await
}

/// `POST /v1/responses` (spec 70 §3.16).
pub async fn responses_handler(
    State(state): State<AppState>,
    method: Method,
    bytes: axum::body::Bytes,
) -> Response {
    proxy_entry(state, ClientProtocol::Responses, method, bytes).await
}

/// `POST /v1/messages` (spec 70 §3.17).
pub async fn messages_handler(
    State(state): State<AppState>,
    method: Method,
    bytes: axum::body::Bytes,
) -> Response {
    proxy_entry(state, ClientProtocol::AnthropicMessages, method, bytes).await
}

/// Shared proxy entry: method check, JSON-object gate, then the proxy.
async fn proxy_entry(
    state: AppState,
    protocol: ClientProtocol,
    method: Method,
    bytes: axum::body::Bytes,
) -> Response {
    if method != Method::POST {
        return not_found();
    }
    let Some(body) = body::parse_json_object(&bytes) else {
        let err = GatewayError::request(body::INVALID_JSON_MESSAGE);
        return body::json_response(StatusCode::BAD_REQUEST, protocol.error_body(&err));
    };
    let stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    if stream {
        match crate::proxy::handle_stream(&state.services, protocol, &body).await {
            Ok(stream) => sse_response(stream),
            Err(err) => body::json_response(
                StatusCode::from_u16(err.http_status()).unwrap_or(StatusCode::BAD_GATEWAY),
                protocol.error_body(&err),
            ),
        }
    } else {
        match crate::proxy::handle_non_stream(&state.services, protocol, &body).await {
            Ok(outcome) => body::text_response(
                StatusCode::from_u16(outcome.status).unwrap_or(StatusCode::OK),
                outcome.content_type,
                outcome.body,
            ),
            Err(err) => body::json_response(
                StatusCode::from_u16(err.http_status()).unwrap_or(StatusCode::BAD_GATEWAY),
                protocol.error_body(&err),
            ),
        }
    }
}

/// The committed SSE response (spec 16 §1). Status 200 with the fixed headers;
/// the body stream itself carries every later frame.
fn sse_response(stream: crate::proxy::stream::SseStream) -> Response {
    let body = axum::body::Body::from_stream(stream);
    let mut response = Response::new(body);
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream"),
    );
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    response
}

/// `GET /v1/messages/count_tokens…` (prefix, any method; spec 70 §3.5).
pub async fn count_tokens_handler() -> Response {
    body::json_response(StatusCode::OK, br#"{"input_tokens":0}"#.to_vec())
}

/// `GET /zed/accounts` (spec 70 §3.6).
pub async fn accounts_handler(State(state): State<AppState>, method: Method) -> Response {
    if method != Method::GET {
        return not_found();
    }
    let now = super::server::now_unix();
    body::json_response(
        StatusCode::OK,
        crate::accounts::status::accounts_body(&state.services.accounts, now),
    )
}

/// `GET /zed/accounts/status` (spec 70 §3.7).
pub async fn accounts_status_handler(State(state): State<AppState>, method: Method) -> Response {
    if method != Method::GET {
        return not_found();
    }
    let now = super::server::now_unix();
    let body = crate::accounts::status::status_body(
        &state.services.accounts,
        state.services.upstream.as_ref(),
        now,
    )
    .await;
    body::json_response(StatusCode::OK, body)
}

/// `POST /zed/accounts/health` (spec 70 §3.8, spec 80). Probes run one at a
/// time, bypass failover, and never change the HTTP status on failure.
pub async fn accounts_health_handler(
    State(state): State<AppState>,
    method: Method,
    bytes: axum::body::Bytes,
) -> Response {
    if method != Method::POST {
        return not_found();
    }
    let map = if bytes.is_empty() {
        serde_json::Map::new()
    } else {
        match body::parse_json(&bytes) {
            Some(Value::Object(map)) => map,
            Some(_) => {
                return body::json_response(
                    StatusCode::BAD_REQUEST,
                    body::NOT_OBJECT_MANAGEMENT.as_bytes().to_vec(),
                )
            }
            None => {
                return body::json_response(
                    StatusCode::BAD_REQUEST,
                    body::INVALID_JSON_MANAGEMENT.as_bytes().to_vec(),
                )
            }
        }
    };
    let account = match map.get("account") {
        None | Some(Value::Null) => None,
        Some(Value::String(name)) => Some(name.clone()),
        Some(_) => return management_bad_request("account must be a string"),
    };
    let model = match map.get("model") {
        None | Some(Value::Null) => None,
        Some(Value::String(model)) => Some(model.clone()),
        Some(_) => return management_bad_request("model must be a string"),
    };
    let services = &state.services;
    let catalog = services.catalog.snapshot();
    let model = model.unwrap_or_else(|| crate::probe::DEFAULT_PROBE_MODEL.to_string());
    if !crate::probe::is_known_probe_model(&catalog, &model) {
        return management_bad_request("unknown model");
    }
    let plan = match crate::probe::probe_plan(&catalog, &model) {
        Ok(plan) => plan,
        Err(err) => return management_error(err),
    };
    let targets = match account {
        Some(name) => {
            if services.accounts.snapshot(&name).is_none() {
                return body::json_response(
                    StatusCode::NOT_FOUND,
                    br#"{"error":"account not found"}"#.to_vec(),
                );
            }
            vec![name]
        }
        None => services.accounts.names(),
    };
    let mut rows = String::new();
    for (index, name) in targets.iter().enumerate() {
        let _ = crate::probe::run_planned_probe(services, name, &plan, &catalog).await;
        if index > 0 {
            rows.push(',');
        }
        push_probe_row(&mut rows, services, name);
    }
    let checked_at = super::server::now_unix();
    let effort = match plan.effort.report.as_deref() {
        Some(value) => match serde_json::to_string(value) {
            Ok(text) => text,
            Err(_) => "null".to_string(),
        },
        None => "null".to_string(),
    };
    let budget = match crate::probe::reported_budget(plan.max_output_tokens) {
        Some(value) => value.to_string(),
        None => "null".to_string(),
    };
    let model_json = serde_json::to_string(&plan.model).unwrap_or_else(|_| "\"\"".to_string());
    let text = format!(
        "{{\"checked_at\":{checked_at},\"probe\":{{\"model\":{model_json},\"reasoning_effort\":{effort},\"max_output_tokens\":{budget}}},\"accounts\":[{rows}]}}"
    );
    body::json_response(StatusCode::OK, text.into_bytes())
}

/// One `accounts[]` entry of the probe response (spec 80 §3).
fn push_probe_row(out: &mut String, services: &crate::proxy::Services, name: &str) {
    let Some(snapshot) = services.accounts.snapshot(name) else {
        return;
    };
    let now = super::server::now_unix();
    let healthy = now >= snapshot.disabled_until;
    let cooldown = (snapshot.disabled_until - now).max(0);
    out.push_str("{\"name\":");
    out.push_str(&serde_json::to_string(&snapshot.name).unwrap_or_else(|_| "\"\"".to_string()));
    out.push_str(&format!(
        ",\"model_ok\":{},\"model_state\":\"{}\",\"model_checked_at\":{},\"model_latency_ms\":{},\"scheduler_healthy\":{},\"cooldown_s\":{},\"last_status\":{}}}",
        snapshot.model_state == crate::accounts::AccountModelState::Healthy,
        snapshot.model_state.as_str(),
        snapshot.model_checked_at,
        snapshot.model_latency_ms,
        healthy,
        cooldown,
        snapshot.last_status
    ));
}

/// `POST /zed/accounts/orb` (spec 70 §3.9). The link is validated before it is
/// written and never echoed back.
pub async fn accounts_orb_handler(
    State(state): State<AppState>,
    method: Method,
    bytes: axum::body::Bytes,
) -> Response {
    if method != Method::POST {
        return not_found();
    }
    let Some(value) = body::parse_json(&bytes) else {
        return management_bad_request("invalid json");
    };
    let Value::Object(map) = value else {
        return body::json_response(
            StatusCode::BAD_REQUEST,
            body::NOT_OBJECT_MANAGEMENT.as_bytes().to_vec(),
        );
    };
    let Some(account) = map.get("account") else {
        return management_bad_request("missing account");
    };
    let Some(name) = account.as_str() else {
        return management_bad_request("account must be a string");
    };
    // An absent `url` clears the link, same as an empty string (spec 50 §3.2).
    let url = match map.get("url") {
        None | Some(Value::Null) => "",
        Some(Value::String(text)) => text.as_str(),
        Some(_) => return management_bad_request("url must be a string"),
    };
    let stored = if url.is_empty() {
        None
    } else {
        match crate::orb::validate_link(url) {
            Ok(link) => Some(link),
            Err(_) => return management_bad_request("invalid orb link"),
        }
    };
    match state.services.accounts.set_orb_url(name, stored.as_deref()) {
        Ok(()) => body::json_response(StatusCode::OK, br#"{"success":true}"#.to_vec()),
        Err(crate::accounts::AccountError::NotFound) => body::json_response(
            StatusCode::NOT_FOUND,
            br#"{"error":"account not found"}"#.to_vec(),
        ),
        Err(_) => body::json_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            br#"{"error":"failed to save orb link"}"#.to_vec(),
        ),
    }
}

/// `POST /zed/accounts/orb/refresh` (spec 70 §3.11, spec 65 §6).
pub async fn accounts_orb_refresh_handler(
    State(state): State<AppState>,
    method: Method,
    bytes: axum::body::Bytes,
) -> Response {
    if method != Method::POST {
        return not_found();
    }
    match orb_refresh_entry(&state, &bytes).await {
        Ok(response) => response,
        Err(err) => management_error(err),
    }
}

async fn orb_refresh_entry(state: &AppState, bytes: &[u8]) -> Result<Response, GatewayError> {
    let body = if bytes.is_empty() {
        Value::Object(Default::default())
    } else {
        match body::parse_json(bytes) {
            Some(value) if value.is_object() => value,
            Some(_) => {
                return Ok(body::json_response(
                    StatusCode::BAD_REQUEST,
                    body::NOT_OBJECT_MANAGEMENT.as_bytes().to_vec(),
                ))
            }
            None => {
                return Ok(body::json_response(
                    StatusCode::BAD_REQUEST,
                    body::INVALID_JSON_MANAGEMENT.as_bytes().to_vec(),
                ))
            }
        }
    };
    // Portal failures are still 200 with `credit.ok=false` (spec 65 §6.4).
    Ok(match crate::orb::refresh(&state.services, &body).await {
        crate::orb::RefreshOutcome::Ok(response) => body::json_response(StatusCode::OK, response),
        crate::orb::RefreshOutcome::BadRequest(message) => management_bad_request(message),
        crate::orb::RefreshOutcome::NotFound => body::json_response(
            StatusCode::NOT_FOUND,
            br#"{"error":"account not found"}"#.to_vec(),
        ),
    })
}

/// `POST /zed/accounts/switch` (spec 70 §3.10). Every malformed body is a 400;
/// 0.2.2 crashed on non-object bodies (spec 50 §10 A1).
pub async fn accounts_switch_handler(
    State(state): State<AppState>,
    method: Method,
    bytes: axum::body::Bytes,
) -> Response {
    if method != Method::POST {
        return not_found();
    }
    let Some(value) = body::parse_json(&bytes) else {
        return management_bad_request("invalid json");
    };
    let Value::Object(map) = value else {
        return body::json_response(
            StatusCode::BAD_REQUEST,
            body::NOT_OBJECT_MANAGEMENT.as_bytes().to_vec(),
        );
    };
    let Some(account) = map.get("account") else {
        return management_bad_request("missing account");
    };
    let Some(name) = account.as_str() else {
        return management_bad_request("bad type");
    };
    match state.services.accounts.switch(name) {
        Ok(()) => body::json_response(StatusCode::OK, br#"{"success":true}"#.to_vec()),
        Err(crate::accounts::AccountError::NotFound) => {
            body::json_response(StatusCode::NOT_FOUND, br#"{"error":"not found"}"#.to_vec())
        }
        Err(err) => management_error(crate::accounts::AccountStore::save_error_body(&err)),
    }
}

fn management_bad_request(message: &str) -> Response {
    let quoted = serde_json::to_string(message).unwrap_or_else(|_| "\"\"".to_string());
    body::json_response(
        StatusCode::BAD_REQUEST,
        format!("{{\"error\":{quoted}}}").into_bytes(),
    )
}

/// `POST /zed/models/refresh` (spec 70 §3.12, spec 15 §5).
pub async fn models_refresh_handler(
    State(state): State<AppState>,
    method: Method,
    bytes: axum::body::Bytes,
) -> Response {
    if method != Method::POST {
        return not_found();
    }
    if !bytes.is_empty() {
        match body::parse_json(&bytes) {
            Some(value) if value.is_object() => {}
            Some(_) => {
                return body::json_response(
                    StatusCode::BAD_REQUEST,
                    body::NOT_OBJECT_MANAGEMENT.as_bytes().to_vec(),
                )
            }
            None => {
                return body::json_response(
                    StatusCode::BAD_REQUEST,
                    body::INVALID_JSON_MANAGEMENT.as_bytes().to_vec(),
                )
            }
        }
    }
    let services = &state.services;
    let now = super::server::now_unix();
    let count = services.catalog.len();

    let Some(account) = first_account(services) else {
        return refresh_body(StatusCode::OK, false, None, count);
    };
    // Serialise pulls, then reserve the 5 s window: a concurrent or recent
    // refresh answers with the current count instead of hitting the upstream.
    let _guard = services.catalog.lock_pull().await;
    let retry_after = services.catalog.reserve_refresh(now);
    if retry_after > 0 {
        let count = services.catalog.len();
        return refresh_body(StatusCode::OK, false, Some(retry_after), count);
    }
    match refresh_catalog(services, &account).await {
        Ok(()) => {
            let count = services.catalog.len();
            services.catalog.rebuild_models(now);
            refresh_body(StatusCode::OK, true, None, count)
        }
        Err(err) => {
            eprintln!("[zed] models refresh failed: {}", err.message);
            // Old registry and cache stay untouched (spec 15 §5).
            body::json_response(
                StatusCode::BAD_GATEWAY,
                br#"{"error":"models refresh failed","refreshed":false}"#.to_vec(),
            )
        }
    }
}

/// Fixed key order for `/zed/models/refresh` (spec 15 §5, spec 70 §3.12).
fn refresh_body(
    status: StatusCode,
    refreshed: bool,
    retry_after_s: Option<i64>,
    count: usize,
) -> Response {
    let text = match retry_after_s {
        Some(retry) => format!(r#"{{"refreshed":false,"retry_after_s":{retry},"count":{count}}}"#),
        None => format!(r#"{{"refreshed":{refreshed},"count":{count}}}"#),
    };
    body::json_response(status, text.into_bytes())
}

/// `GET /zed/usage` (spec 70 §3.13). Returns the active account's LLM-token
/// JWT payload bytes exactly as the token carries them, never the full token
/// or its signature.
pub async fn usage_handler(State(state): State<AppState>, method: Method) -> Response {
    if method != Method::GET {
        return not_found();
    }
    let services = &state.services;
    let Some(account) = current_account(services) else {
        return no_account();
    };
    let now = super::server::now_unix();
    match crate::proxy::account_token(services, &account, now).await {
        Ok(token) => match crate::upstream::jwt_payload_bytes(&token) {
            Some(payload) => body::json_response(StatusCode::OK, payload),
            None => internal_error(),
        },
        Err(_) => internal_error(),
    }
}

/// `GET /zed/billing` (spec 70 §3.14). The upstream body is passed through
/// unchanged; non-2xx, transport failure and over-cap bodies are 502.
pub async fn billing_handler(State(state): State<AppState>, method: Method) -> Response {
    if method != Method::GET {
        return not_found();
    }
    let services = &state.services;
    let Some(account) = current_account(services) else {
        return no_account();
    };
    let request = crate::upstream::token::me_request(&account.user_id, &account.credential);
    // The upstream client already enforces the 1 MiB meta cap (spec 70 §6).
    match services.upstream.execute(request).await {
        Ok(response) if response.is_success() => body::json_response(StatusCode::OK, response.body),
        _ => body::json_response(
            StatusCode::BAD_GATEWAY,
            br#"{"error":"failed to fetch user info"}"#.to_vec(),
        ),
    }
}

/// The active account, or `None` when no account is selected (spec 70 §3.13
/// and §3.14 answer 400 rather than falling back to the first account).
fn current_account(services: &Services) -> Option<crate::accounts::AccountSnapshot> {
    let name = services.accounts.current_name();
    if name.is_empty() {
        return None;
    }
    services.accounts.snapshot(&name)
}

fn no_account() -> Response {
    body::json_response(
        StatusCode::BAD_REQUEST,
        br#"{"error":"no account"}"#.to_vec(),
    )
}

fn internal_error() -> Response {
    body::json_response(
        StatusCode::INTERNAL_SERVER_ERROR,
        br#"{"error":"internal error"}"#.to_vec(),
    )
}

/// `POST /zed/login` (spec 70 §3.18). A non-JSON or non-object body is treated
/// as empty, never as an error (spec 60 §5.1 item 1).
pub async fn login_start_handler(
    State(state): State<AppState>,
    method: Method,
    bytes: axum::body::Bytes,
) -> Response {
    if method != Method::POST {
        return not_found();
    }
    let value = body::parse_json(&bytes).unwrap_or(Value::Null);
    let mode = value.get("mode").and_then(Value::as_str);
    let name = value
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty());
    let result = if mode == Some("paste") {
        crate::login::start_paste_login(&state.services, name).await
    } else {
        crate::login::start_local_login(state.services.clone(), name).await
    };
    match result {
        Ok(body) => body::json_response(StatusCode::OK, body),
        Err(err) => login_error_response(err),
    }
}

fn login_error_response(err: GatewayError) -> Response {
    let (status, body) = crate::login::error_response(&err);
    body::json_response(
        StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        body,
    )
}

/// `POST /zed/login/complete` (spec 70 §3.19).
pub async fn login_complete_handler(
    State(state): State<AppState>,
    method: Method,
    bytes: axum::body::Bytes,
) -> Response {
    if method != Method::POST {
        return not_found();
    }
    let Some(value) = body::parse_json(&bytes) else {
        return management_bad_request("invalid json");
    };
    match crate::login::complete_paste_login(&state.services, &value).await {
        Ok(body) => body::json_response(StatusCode::OK, body),
        Err(err) => login_error_response(err),
    }
}

/// `GET /zed/login/status` (spec 70 §3.20). `success` / `failed` are consumed
/// on read (spec 60 §8).
pub async fn login_status_handler(State(_state): State<AppState>, method: Method) -> Response {
    if method != Method::GET {
        return not_found();
    }
    body::json_response(
        StatusCode::OK,
        crate::login::status_body(crate::login::take_status()),
    )
}

/// Fallback: the count_tokens prefix, then 404 (spec 70 §1, §2.1). `OPTIONS`
/// never gets here; the entry guard answers it.
pub async fn fallback_handler(request: Request<axum::body::Body>) -> Response {
    if request.uri().path().starts_with(COUNT_TOKENS_PREFIX) {
        return count_tokens_handler().await;
    }
    not_found()
}

/// Management endpoints use the simple error body (spec 70 §8).
pub fn management_error(err: GatewayError) -> Response {
    let message = serde_json::to_string(&err.message).unwrap_or_else(|_| "\"\"".to_string());
    body::json_response(
        StatusCode::from_u16(err.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        format!("{{\"error\":{message}}}").into_bytes(),
    )
}
