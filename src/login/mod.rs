//! Login: RSA keypair, authorization URL, callback parsing, OAEP-SHA256
//! decryption and the single process-wide login slot (spec `60-login.md`).
//!
//! The private key lives only in memory (`RsaPrivateKey` is `ZeroizeOnDrop`, so
//! dropping it wipes the private exponent and primes) and never reaches a file,
//! a log, a response or a child process. There is no `openssl` call and no
//! temporary ciphertext file (spec 60 §2, §10).

use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use base64::alphabet;
use base64::engine::general_purpose::{GeneralPurpose, URL_SAFE_NO_PAD};
use base64::engine::{DecodePaddingMode, Engine, GeneralPurposeConfig};
use rsa::pkcs1::EncodeRsaPublicKey;
use rsa::{Oaep, RsaPrivateKey, RsaPublicKey};
use serde_json::Value;
use sha2::Sha256;

use crate::accounts::AccountStore;
use crate::error::{ErrorKind, GatewayError};
use crate::proxy::Services;

/// Authorization is valid for ten minutes (spec 60 §1).
pub const LOGIN_TTL_SECS: i64 = 600;
/// Pasted callbacks are limited to 8192 bytes (spec 60 §6.1).
pub const MAX_CALLBACK_BYTES: usize = 8192;
/// Zed's fixed success redirect (spec 60 §5.4 item 7).
pub const SUCCESS_REDIRECT: &str = "https://zed.dev/native_app_signin_succeeded";

/// Detail line of the "data directory is not writable" 500 (spec 60 §5.1).
pub const WRITABLE_DETAIL: &str =
    "accounts.json cannot be saved; on Linux run chown -R 10001:10001 ./data or set ZED_DATA_DIR";
/// `GatewayError.code` marker for that two-key body.
pub const DATA_DIR_ERROR_CODE: &str = "data_dir_not_writable";

const MAX_NAME_BYTES: usize = 256;

/// base64url that also accepts `=` padding when decoding (spec 60 §2.3).
const URL_SAFE_ANY_PADDING: GeneralPurpose = GeneralPurpose::new(
    &alphabet::URL_SAFE,
    GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
);

/// Visible state of `GET /zed/login/status` (spec 60 §8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LoginStatus {
    #[default]
    Idle,
    Waiting,
    Success,
    Failed,
}

impl LoginStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            LoginStatus::Idle => "idle",
            LoginStatus::Waiting => "waiting",
            LoginStatus::Success => "success",
            LoginStatus::Failed => "failed",
        }
    }
}

/// Key and display name of the flow holding the slot. Dropping it wipes the
/// private key (spec 60 §2.5).
#[derive(Debug)]
struct PendingLogin {
    key: RsaPrivateKey,
    name: String,
}

/// The single login slot (spec 60 §4). Only one flow runs at a time; the
/// generation counter lets a late cleanup only affect its own attempt.
#[derive(Debug, Default)]
pub struct LoginSlot {
    active: bool,
    deadline: i64,
    generation: u64,
    status: LoginStatus,
    pending: Option<PendingLogin>,
}

impl LoginSlot {
    pub fn new() -> Self {
        Self::default()
    }

    #[cfg(test)]
    pub fn status(&self) -> LoginStatus {
        self.status
    }

    /// Read-and-consume for `success` / `failed` (spec 60 §8).
    pub fn take_status(&mut self) -> LoginStatus {
        let status = self.status;
        if matches!(status, LoginStatus::Success | LoginStatus::Failed) {
            self.status = LoginStatus::Idle;
        }
        status
    }

    /// Expire the slot when its deadline has passed; returns true once
    /// (spec 60 §4 rule 2). `deadline` is kept so the paste flow can still
    /// answer 410, and a waiting flow turns `failed` (spec 60 §8).
    pub fn expire_if_due(&mut self, now: i64) -> bool {
        if self.active && now >= self.deadline {
            self.active = false;
            self.pending = None;
            if self.status == LoginStatus::Waiting {
                self.status = LoginStatus::Failed;
            }
            true
        } else {
            false
        }
    }

    /// Try to occupy; `Err` means another flow is running (409).
    pub fn occupy(&mut self, now: i64, ttl: i64) -> Result<u64, GatewayError> {
        self.expire_if_due(now);
        if self.active {
            let mut err = GatewayError::new(ErrorKind::Request, "login already in progress");
            err.status = Some(409);
            return Err(err);
        }
        self.active = true;
        self.deadline = now + ttl;
        self.generation = self.generation.wrapping_add(1);
        self.status = LoginStatus::Waiting;
        self.pending = None;
        Ok(self.generation)
    }

    /// Release only if the generation still matches (spec 60 §4 rule 3).
    pub fn release(&mut self, generation: u64) {
        if self.active && self.generation == generation {
            self.active = false;
            self.deadline = 0;
            self.pending = None;
        }
    }

    #[cfg(test)]
    pub fn is_active(&self) -> bool {
        self.active
    }

    pub fn deadline(&self) -> i64 {
        self.deadline
    }
}

/// Process-wide slot; `LoginSlot` itself stays constructible for tests.
fn slot() -> &'static Mutex<LoginSlot> {
    static SLOT: OnceLock<Mutex<LoginSlot>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(LoginSlot::new()))
}

fn lock_slot() -> MutexGuard<'static, LoginSlot> {
    slot()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Read-and-consume for `GET /zed/login/status` (spec 60 §8).
pub fn take_status() -> LoginStatus {
    lock_slot().take_status()
}

/// `https://zed.dev/native_app_signin?native_app_port=<port>&native_app_public_key=<b64url>`
/// (spec 60 §3).
pub fn login_url(port: u16, public_key_b64url: &str) -> String {
    format!(
        "https://zed.dev/native_app_signin?native_app_port={port}&native_app_public_key={public_key_b64url}"
    )
}

/// Extract `user_id` and `access_token` from a pasted callback (spec 60 §6).
pub fn parse_callback(input: &str) -> Result<CallbackParams, LoginError> {
    if input.len() > MAX_CALLBACK_BYTES {
        return Err(LoginError::TooLong);
    }
    let trimmed = input.trim_matches(|c: char| matches!(c, ' ' | '\t' | '\r' | '\n'));
    let without_fragment = match trimmed.find('#') {
        Some(index) => &trimmed[..index],
        None => trimmed,
    };
    let query = if let Some(index) = without_fragment.find('?') {
        &without_fragment[index + 1..]
    } else if without_fragment.contains('=') {
        without_fragment
    } else {
        return Err(LoginError::NotACallback);
    };
    parse_query(query)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallbackParams {
    pub user_id: String,
    pub access_token: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginError {
    TooLong,
    MissingUserId,
    MissingAccessToken,
    NotACallback,
    DecryptFailed,
    CredentialNotJson,
    /// Carries the short, credential-free error name (spec 60 §5.2 step 7).
    SaveFailed(&'static str),
    NoPendingLogin,
    Expired,
}

impl LoginError {
    pub fn message(self) -> &'static str {
        match self {
            LoginError::TooLong => "invalid callback url: too long",
            LoginError::MissingUserId => "invalid callback url: missing user_id",
            LoginError::MissingAccessToken => "invalid callback url: missing access_token",
            LoginError::NotACallback => "invalid callback url: no user_id/access_token query",
            LoginError::DecryptFailed => {
                "failed to decrypt access token (wrong key or corrupted link)"
            }
            LoginError::CredentialNotJson => "decrypted credential is not valid JSON",
            LoginError::SaveFailed(_) => "failed to save account",
            LoginError::NoPendingLogin => "no pending login",
            LoginError::Expired => "pending login expired; start again",
        }
    }

    pub fn http_status(self) -> u16 {
        match self {
            LoginError::NoPendingLogin => 409,
            LoginError::Expired => 410,
            LoginError::SaveFailed(_) => 500,
            _ => 400,
        }
    }

    fn to_gateway(self) -> GatewayError {
        let message = match self {
            LoginError::SaveFailed(short) => format!("failed to save account: {short}"),
            other => other.message().to_string(),
        };
        let mut err = GatewayError::new(ErrorKind::Request, message);
        err.status = Some(self.http_status());
        err
    }
}

/// Status code and body for a login failure (spec 70 §3.18, §3.19). Anything
/// without an explicit status is an internal failure (key/listener), so 500.
pub fn error_response(err: &GatewayError) -> (u16, Vec<u8>) {
    if err.code == Some(DATA_DIR_ERROR_CODE) {
        let detail = serde_json::to_string(WRITABLE_DETAIL).unwrap_or_default();
        return (
            500,
            format!("{{\"error\":\"data directory is not writable\",\"detail\":{detail}}}")
                .into_bytes(),
        );
    }
    let quoted = serde_json::to_string(&err.message).unwrap_or_else(|_| "\"\"".to_string());
    (
        err.status.unwrap_or(500),
        format!("{{\"error\":{quoted}}}").into_bytes(),
    )
}

fn bad_request(message: &str) -> GatewayError {
    let mut err = GatewayError::new(ErrorKind::Request, message);
    err.status = Some(400);
    err
}

// ---------------------------------------------------------------------------
// Crypto (spec 60 §2)
// ---------------------------------------------------------------------------

fn now_unix() -> i64 {
    crate::http::server::now_unix()
}

fn generate_key() -> Result<RsaPrivateKey, GatewayError> {
    RsaPrivateKey::new(&mut rand::thread_rng(), 2048)
        .map_err(|_| GatewayError::internal("failed to generate RSA key"))
}

/// PKCS#1 `RSAPublicKey` DER, base64url without padding (spec 60 §2.2).
fn export_public_key(private: &RsaPrivateKey) -> Result<String, GatewayError> {
    let public = RsaPublicKey::from(private);
    let der = public
        .to_pkcs1_der()
        .map_err(|_| GatewayError::internal("failed to export public key"))?;
    Ok(URL_SAFE_NO_PAD.encode(der.as_bytes()))
}

/// base64url → RSA-OAEP-SHA256 decrypt. Every failure is the same error so the
/// endpoint cannot serve as a padding oracle (spec 60 §2.3, §2.4).
fn decrypt_token(private: &RsaPrivateKey, access_token: &str) -> Result<Vec<u8>, LoginError> {
    let ciphertext = URL_SAFE_ANY_PADDING
        .decode(access_token.as_bytes())
        .map_err(|_| LoginError::DecryptFailed)?;
    if ciphertext.len() != 256 {
        return Err(LoginError::DecryptFailed);
    }
    private
        .decrypt(Oaep::new::<Sha256>(), &ciphertext)
        .map_err(|_| LoginError::DecryptFailed)
}

// ---------------------------------------------------------------------------
// Callback parsing (spec 60 §6)
// ---------------------------------------------------------------------------

fn parse_query(query: &str) -> Result<CallbackParams, LoginError> {
    let mut user_id: Option<String> = None;
    let mut access_token: Option<String> = None;
    for pair in query.split('&') {
        let Some(equals) = pair.find('=') else {
            continue;
        };
        let value = percent_decode(&pair[equals + 1..]);
        match &pair[..equals] {
            "user_id" => user_id = Some(value),
            "access_token" => access_token = Some(value),
            _ => {}
        }
    }
    let user_id = user_id
        .filter(|value| !value.is_empty())
        .ok_or(LoginError::MissingUserId)?;
    let access_token = access_token
        .filter(|value| !value.is_empty())
        .ok_or(LoginError::MissingAccessToken)?;
    Ok(CallbackParams {
        user_id,
        access_token,
    })
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// WHATWG-style byte decode: `%XX`, `+` → space, a stray `%` stays literal
/// (spec 60 §6.4). `shortcut: non-UTF-8 bytes become U+FFFD, the contract
/// exposes String values; upgrade if user_id ever needs raw bytes.`
fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' if index + 2 < bytes.len() => {
                match (hex_value(bytes[index + 1]), hex_value(bytes[index + 2])) {
                    (Some(high), Some(low)) => {
                        out.push(high << 4 | low);
                        index += 3;
                    }
                    _ => {
                        out.push(b'%');
                        index += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

// ---------------------------------------------------------------------------
// Account write (spec 60 §7)
// ---------------------------------------------------------------------------

/// Parse the decrypted credential, name the account and persist it. Callers
/// hold the slot lock, so a superseded flow can never overwrite a newer one
/// (spec 60 §4 rule 7).
pub(crate) fn save_account(
    store: &AccountStore,
    user_id: &str,
    plain: &[u8],
    stored_name: Option<&str>,
) -> Result<String, GatewayError> {
    let credential: Value =
        serde_json::from_slice(plain).map_err(|_| LoginError::CredentialNotJson.to_gateway())?;
    let name = match stored_name {
        Some(name) if !name.is_empty() => name,
        _ => user_id,
    };
    if name.len() > MAX_NAME_BYTES || user_id.len() > MAX_NAME_BYTES {
        return Err(bad_request(
            "account name or user_id is too long (max 256 bytes)",
        ));
    }
    store
        .add_account(name, user_id, credential)
        .map_err(|err| LoginError::SaveFailed(err.short_name()).to_gateway())?;
    Ok(name.to_string())
}

/// Reload the pool and make the new account current (spec 60 §5.2 item 8).
fn finish_success(store: &AccountStore, name: &str) {
    if store.reload().is_ok() {
        let _ = store.switch(name);
    }
}

fn data_dir_writable(dir: &Path) -> bool {
    let path = dir.join(".write_probe");
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let created = options.open(&path).and_then(|file| file.sync_all());
    let _ = fs::remove_file(&path);
    created.is_ok()
}

fn writable_error() -> GatewayError {
    let mut err = GatewayError::internal("data directory is not writable");
    err.code = Some(DATA_DIR_ERROR_CODE);
    err.status = Some(500);
    err
}

// ---------------------------------------------------------------------------
// Flows
// ---------------------------------------------------------------------------

/// Start a paste flow; returns the response body (spec 60 §5.1).
pub async fn start_paste_login(
    services: &Services,
    name: Option<&str>,
) -> Result<Vec<u8>, GatewayError> {
    if !data_dir_writable(services.accounts.data_dir()) {
        return Err(writable_error());
    }
    let port = crate::config::login_port_from_env().map_err(|()| {
        let mut err = GatewayError::new(ErrorKind::Request, "ZED_LOGIN_PORT must be 1-65535");
        err.status = Some(400);
        err
    })?;
    // Key first: a generation failure must not occupy the slot (spec 60 §2.1).
    let key = generate_key()?;
    let public = export_public_key(&key)?;
    let now = now_unix();
    let generation = {
        let mut slot = lock_slot();
        let generation = slot.occupy(now, LOGIN_TTL_SECS)?;
        slot.pending = Some(PendingLogin {
            key,
            name: name.unwrap_or_default().to_string(),
        });
        generation
    };
    spawn_expiry(generation, now + LOGIN_TTL_SECS);
    let url = login_url(port, &public);
    eprintln!("[login] paste flow started (port {port})");
    Ok(format!(
        "{{\"login_url\":{},\"port\":{port},\"mode\":\"paste\",\"expires_in\":{LOGIN_TTL_SECS}}}",
        serde_json::to_string(&url).unwrap_or_else(|_| "\"\"".to_string())
    )
    .into_bytes())
}

/// Release the slot once its deadline passes (spec 60 §4 rules 3 and 4). A
/// replaced flow is left alone because its generation moved on.
fn spawn_expiry(generation: u64, expires_at: i64) {
    tokio::spawn(async move {
        let wait = expires_at.saturating_sub(now_unix()).max(0) as u64;
        tokio::time::sleep(Duration::from_secs(wait)).await;
        let mut slot = lock_slot();
        if slot.active && slot.generation == generation {
            slot.expire_if_due(now_unix());
        }
    });
}

/// Start a local-callback flow; returns the response body (spec 60 §5.4).
///
/// Takes `Arc<Services>` because the accept loop runs on its own thread and
/// must keep the account store alive after the handler returns.
pub async fn start_local_login(
    services: Arc<Services>,
    name: Option<&str>,
) -> Result<Vec<u8>, GatewayError> {
    if !data_dir_writable(services.accounts.data_dir()) {
        return Err(writable_error());
    }
    let key = generate_key()?;
    let public = export_public_key(&key)?;
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .map_err(|err| GatewayError::internal(format!("cannot bind login listener: {err}")))?;
    let port = listener
        .local_addr()
        .map_err(|err| GatewayError::internal(format!("cannot read login port: {err}")))?
        .port();

    let now = now_unix();
    let generation = {
        let mut slot = lock_slot();
        let generation = slot.occupy(now, LOGIN_TTL_SECS)?;
        slot.pending = Some(PendingLogin {
            key,
            name: name.unwrap_or_default().to_string(),
        });
        generation
    };
    let url = login_url(port, &public);
    eprintln!("[login] local callback flow started (port {port})");
    let expires_at = now + LOGIN_TTL_SECS;
    std::thread::spawn(move || run_local_worker(services, listener, generation, expires_at));
    Ok(format!(
        "{{\"login_url\":{},\"port\":{port}}}",
        serde_json::to_string(&url).unwrap_or_else(|_| "\"\"".to_string())
    )
    .into_bytes())
}

/// Accept one callback before `expires_at`, decrypt it and save the account
/// (spec 60 §5.4 items 6–10).
fn run_local_worker(
    services: Arc<Services>,
    listener: TcpListener,
    generation: u64,
    expires_at: i64,
) {
    let Some((mut stream, params)) = wait_for_callback(&listener, expires_at) else {
        fail_local(generation, "timed out");
        return;
    };
    let pending = {
        let mut slot = lock_slot();
        if !slot.active || slot.generation != generation {
            return; // superseded: drop the credential without touching disk
        }
        match slot.pending.take() {
            Some(pending) => pending,
            None => {
                slot.release(generation);
                slot.status = LoginStatus::Failed;
                return;
            }
        }
    };
    let plain = match decrypt_token(&pending.key, &params.access_token) {
        Ok(plain) => {
            write_redirect(&mut stream);
            plain
        }
        Err(_) => {
            fail_local(generation, "decrypt failed");
            return;
        }
    };
    let stored = pending.name.clone();
    drop(pending);
    drop(stream);

    let mut slot = lock_slot();
    if !slot.active || slot.generation != generation {
        return;
    }
    let name = match save_account(&services.accounts, &params.user_id, &plain, Some(&stored)) {
        Ok(name) => name,
        Err(err) => {
            slot.release(generation);
            slot.status = LoginStatus::Failed;
            eprintln!("[login] failed: {}", err.message);
            return;
        }
    };
    slot.release(generation);
    slot.status = LoginStatus::Success;
    drop(slot);
    finish_success(&services.accounts, &name);
    eprintln!("[login] success: {name}");
}

fn fail_local(generation: u64, reason: &str) {
    {
        let mut slot = lock_slot();
        if slot.active && slot.generation == generation {
            slot.release(generation);
            slot.status = LoginStatus::Failed;
        }
    }
    eprintln!("[login] failed: {reason}");
}

/// Accept one connection before the deadline (spec 60 §5.4 item 10).
fn wait_for_callback(
    listener: &TcpListener,
    expires_at: i64,
) -> Option<(TcpStream, CallbackParams)> {
    listener.set_nonblocking(true).ok()?;
    loop {
        if now_unix() >= expires_at {
            return None;
        }
        match listener.accept() {
            Ok((mut stream, _)) => {
                stream.set_nonblocking(false).ok()?;
                stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
                let target = read_request_target(&mut stream)?;
                let params = callback_from_target(&target).ok()?;
                return Some((stream, params));
            }
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(_) => return None,
        }
    }
}

/// Read the request head (max 8192 bytes) and return its request target
/// (spec 60 §5.4 item 6, §6.3).
fn read_request_target(stream: &mut TcpStream) -> Option<String> {
    let mut buffer: Vec<u8> = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    while buffer.len() < MAX_CALLBACK_BYTES {
        let read = stream.read(&mut chunk).ok()?;
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }
    let head = String::from_utf8_lossy(&buffer);
    let target = head.lines().next()?.split_whitespace().nth(1)?;
    Some(target.to_string())
}

/// Zed's own GET; no "not a callback" tolerance and no 8192 cap (spec 60 §5.4).
fn callback_from_target(target: &str) -> Result<CallbackParams, LoginError> {
    let without_fragment = match target.find('#') {
        Some(index) => &target[..index],
        None => target,
    };
    match without_fragment.find('?') {
        Some(index) => parse_query(&without_fragment[index + 1..]),
        None => parse_query(without_fragment),
    }
}

fn write_redirect(stream: &mut TcpStream) {
    let response = format!(
        "HTTP/1.1 302 Found\r\nLocation: {SUCCESS_REDIRECT}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

/// Complete a paste flow; returns the response body (spec 60 §5.2).
pub async fn complete_paste_login(
    services: &Services,
    body: &Value,
) -> Result<Vec<u8>, GatewayError> {
    let text = match body
        .as_object()
        .and_then(|map| map.get("url").or_else(|| map.get("query")))
    {
        Some(Value::String(text)) => text.as_str(),
        _ => return Err(bad_request("missing url")),
    };

    let mut slot = lock_slot();
    let now = now_unix();
    slot.expire_if_due(now);
    if !slot.active {
        let deadline = slot.deadline();
        drop(slot);
        return Err(if deadline != 0 && now >= deadline {
            LoginError::Expired.to_gateway()
        } else {
            LoginError::NoPendingLogin.to_gateway()
        });
    }
    let Some(pending) = slot.pending.as_ref() else {
        drop(slot);
        return Err(LoginError::NoPendingLogin.to_gateway());
    };
    // Parse and decrypt inside the lock; every failure keeps the slot so the
    // user can fix the link and retry (spec 60 §4 rule 5).
    let params = parse_callback(text).map_err(LoginError::to_gateway)?;
    let plain =
        decrypt_token(&pending.key, &params.access_token).map_err(LoginError::to_gateway)?;
    let stored = pending.name.clone();
    let name = save_account(&services.accounts, &params.user_id, &plain, Some(&stored))?;
    let generation = slot.generation;
    slot.release(generation);
    // The paste flow answers over HTTP, so the polled status goes back to idle
    // rather than staying `waiting` forever (spec 60 §5.2 item 8, §8).
    slot.status = LoginStatus::Idle;
    drop(slot);

    finish_success(&services.accounts, &name);
    eprintln!("[login] success: {name}");
    Ok(format!(
        "{{\"status\":\"success\",\"name\":{}}}",
        serde_json::to_string(&name).unwrap_or_else(|_| "\"\"".to_string())
    )
    .into_bytes())
}

/// Status body for `GET /zed/login/status` (spec 60 §8).
pub fn status_body(status: LoginStatus) -> Vec<u8> {
    format!("{{\"status\":\"{}\"}}", status.as_str()).into_bytes()
}

/// CLI login (`zed-proxy login [name]`, spec 60 §9). Runs before the server
/// starts, so it blocks on the callback itself.
pub fn run_cli_login(name: Option<&str>) -> Result<(), GatewayError> {
    let key = generate_key()?;
    let public = export_public_key(&key)?;
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .map_err(|err| GatewayError::internal(format!("cannot bind login listener: {err}")))?;
    let port = listener
        .local_addr()
        .map_err(|err| GatewayError::internal(format!("cannot read login port: {err}")))?
        .port();
    let url = login_url(port, &public);
    eprintln!("[login] opening browser...");
    eprintln!("  {url}");
    eprintln!("[login] waiting for callback on port {port}...");

    let Some((mut stream, params)) = wait_for_callback(&listener, now_unix() + LOGIN_TTL_SECS)
    else {
        return Err(GatewayError::internal(
            "timed out waiting for login callback",
        ));
    };
    let plain = match decrypt_token(&key, &params.access_token) {
        Ok(plain) => {
            write_redirect(&mut stream);
            plain
        }
        Err(err) => return Err(err.to_gateway()),
    };

    // main entered ZED_DATA_DIR before dispatching, so "." is that directory
    // (spec 60 §9, spec 75 §2.2).
    let store = AccountStore::load(".", now_unix());
    let saved = save_account(&store, &params.user_id, &plain, name)?;
    finish_success(&store, &saved);
    println!("[ok] user_id: {}", params.user_id);
    println!("[ok] saved to accounts.json as '{saved}'");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{Catalog, CatalogHandle};
    use crate::upstream::client::BoxFuture;
    use crate::upstream::client::{UpstreamRequest, UpstreamResponse, UpstreamStream};
    use crate::upstream::UpstreamClient;
    use rsa::pkcs1::DecodeRsaPublicKey;
    use std::path::PathBuf;

    /// The process-wide slot and `ZED_LOGIN_PORT` are global; tests that touch
    /// them take this lock so they never race.
    async fn global_guard() -> tokio::sync::MutexGuard<'static, ()> {
        static GUARD: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
        GUARD
            .get_or_init(|| tokio::sync::Mutex::new(()))
            .lock()
            .await
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "zprs-login-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    struct FakeUpstream;

    impl UpstreamClient for FakeUpstream {
        fn execute<'a>(
            &'a self,
            _request: UpstreamRequest,
        ) -> BoxFuture<'a, Result<UpstreamResponse, GatewayError>> {
            Box::pin(async move {
                Ok(UpstreamResponse {
                    status: 404,
                    body: Vec::new(),
                })
            })
        }

        fn execute_stream<'a>(
            &'a self,
            _request: UpstreamRequest,
        ) -> BoxFuture<'a, Result<UpstreamStream, GatewayError>> {
            Box::pin(async move { Err(GatewayError::internal("not used")) })
        }
    }

    fn services(dir: &Path) -> Services {
        Services {
            catalog: CatalogHandle::new(Catalog::builtin_or_empty()),
            accounts: AccountStore::load(dir, 0),
            upstream: Box::new(FakeUpstream),
        }
    }

    fn keypair() -> (RsaPrivateKey, String) {
        let key = generate_key().unwrap();
        let public = export_public_key(&key).unwrap();
        (key, public)
    }

    fn encrypt(public_b64url: &str, plaintext: &[u8]) -> String {
        let der = URL_SAFE_NO_PAD.decode(public_b64url).unwrap();
        let public = RsaPublicKey::from_pkcs1_der(&der).unwrap();
        let ciphertext = public
            .encrypt(&mut rand::thread_rng(), Oaep::new::<Sha256>(), plaintext)
            .unwrap();
        URL_SAFE_NO_PAD.encode(&ciphertext)
    }

    #[test]
    fn slot_is_exclusive_and_expires_once() {
        let mut slot = LoginSlot::new();
        let generation = slot.occupy(100, LOGIN_TTL_SECS).unwrap();
        assert!(slot.occupy(200, LOGIN_TTL_SECS).is_err());
        // 599 seconds in: still running; 600: expires exactly once.
        assert!(!slot.expire_if_due(699));
        assert!(slot.expire_if_due(700));
        assert!(!slot.expire_if_due(701));
        let next = slot.occupy(700, LOGIN_TTL_SECS).unwrap();
        assert_ne!(next, generation);
        slot.release(generation);
        assert!(
            slot.is_active(),
            "an old generation must not free the new slot"
        );
        slot.release(next);
        assert!(!slot.is_active());
    }

    #[test]
    fn status_is_consumed_on_read() {
        let mut slot = LoginSlot::new();
        assert_eq!(slot.take_status(), LoginStatus::Idle);
        slot.status = LoginStatus::Success;
        assert_eq!(slot.take_status(), LoginStatus::Success);
        assert_eq!(slot.status(), LoginStatus::Idle);
    }

    #[test]
    fn login_url_argument_order_is_fixed() {
        assert_eq!(
            login_url(58443, "pk"),
            "https://zed.dev/native_app_signin?native_app_port=58443&native_app_public_key=pk"
        );
    }

    #[test]
    fn status_bodies_are_single_line() {
        assert_eq!(
            status_body(LoginStatus::Waiting),
            br#"{"status":"waiting"}"#.to_vec()
        );
    }

    #[test]
    fn callback_parse_matrix() {
        let cases: &[(&str, &str, &str)] = &[
            (
                "http://127.0.0.1:58443/?user_id=4242&access_token=abc-DEF_123",
                "4242",
                "abc-DEF_123",
            ),
            ("user_id=4242&access_token=tok", "4242", "tok"),
            ("?user_id=4242&access_token=tok", "4242", "tok"),
            ("  \t user_id=a&access_token=b\r\n", "a", "b"),
            (
                "user_id=1&access_token=t#x?user_id=9&access_token=z",
                "1",
                "t",
            ),
            ("user_id=a+b&access_token=t%7E", "a b", "t~"),
            ("user_id=user%2Bone&access_token=a%22b", "user+one", "a\"b"),
            ("user_id=100%&access_token=t%zz", "100%", "t%zz"),
            ("state=x&code=y&garbage&user_id=u&access_token=t", "u", "t"),
            (
                "user_id=1&access_token=first&user_id=2&access_token=second",
                "2",
                "second",
            ),
            ("User_ID=u&access_token=t&user_id=real", "real", "t"),
        ];
        for (input, user_id, access_token) in cases {
            let parsed = parse_callback(input).unwrap_or_else(|err| panic!("{input}: {err:?}"));
            assert_eq!(parsed.user_id, *user_id, "{input}");
            assert_eq!(parsed.access_token, *access_token, "{input}");
        }
    }

    #[test]
    fn callback_parse_rejections_are_ordered() {
        let missing_user = LoginError::MissingUserId;
        let missing_token = LoginError::MissingAccessToken;
        for input in ["", "not a callback", "http://127.0.0.1:1/"] {
            assert_eq!(
                parse_callback(input),
                Err(LoginError::NotACallback),
                "{input}"
            );
        }
        for input in ["?", "access_token=t"] {
            assert_eq!(parse_callback(input), Err(missing_user), "{input}");
        }
        assert_eq!(parse_callback("user_id=&access_token=t"), Err(missing_user));
        assert_eq!(parse_callback("user_id=u"), Err(missing_token));
        assert_eq!(
            parse_callback("user_id=u&access_token="),
            Err(missing_token)
        );
        assert_eq!(
            parse_callback(&"a".repeat(MAX_CALLBACK_BYTES + 1)),
            Err(LoginError::TooLong)
        );
        let prefix = "user_id=";
        let suffix = "&access_token=t";
        let exactly = format!(
            "{prefix}{}{suffix}",
            "a".repeat(MAX_CALLBACK_BYTES - prefix.len() - suffix.len())
        );
        assert_eq!(exactly.len(), MAX_CALLBACK_BYTES);
        assert!(parse_callback(&exactly).is_ok());
    }

    #[test]
    fn public_key_export_is_pkcs1_der_with_the_spec_shape() {
        let (key, public) = keypair();
        let der = URL_SAFE_NO_PAD.decode(&public).unwrap();
        assert!((270..=294).contains(&der.len()), "len {}", der.len());
        assert_eq!(&der[..2], &[0x30, 0x82], "outer SEQUENCE");
        // modulus INTEGER, 0x81 0x01 length 257 = 0x00 + 256 bytes
        assert_eq!(&der[4..9], &[0x02, 0x82, 0x01, 0x01, 0x00], "modulus");
        assert_eq!(
            &der[der.len() - 5..],
            &[0x02, 0x03, 0x01, 0x00, 0x01],
            "e=65537"
        );
        assert!(!public.contains('='), "no padding");
        // The DER is a usable public key for the same private key.
        let parsed = RsaPublicKey::from_pkcs1_der(&der).unwrap();
        let ciphertext = parsed
            .encrypt(
                &mut rand::thread_rng(),
                Oaep::new::<Sha256>(),
                br#"{"access_token":"x"}"#,
            )
            .unwrap();
        let plain = key.decrypt(Oaep::new::<Sha256>(), &ciphertext).unwrap();
        assert_eq!(plain, br#"{"access_token":"x"}"#);
    }

    #[test]
    fn decrypt_round_trip_accepts_padded_and_unpadded_base64() {
        let (key, public) = keypair();
        let plaintext = br#"{"access_token":"tok","refresh_token":"r"}"#;
        let ciphertext = URL_SAFE_NO_PAD.decode(encrypt(&public, plaintext)).unwrap();
        let canonical = URL_SAFE_NO_PAD.encode(&ciphertext);
        assert_eq!(decrypt_token(&key, &canonical).unwrap(), plaintext);
        let padded = format!("{canonical}=="); // 256 bytes -> 342 chars + padding
        assert_eq!(decrypt_token(&key, &padded).unwrap(), plaintext);
    }

    #[test]
    fn decrypt_rejects_a_foreign_key_and_tampered_ciphertext() {
        let (key, _) = keypair();
        let (other_key, other_public) = keypair();
        let foreign = encrypt(&other_public, b"{}");
        assert_eq!(
            decrypt_token(&key, &foreign).unwrap_err(),
            LoginError::DecryptFailed
        );
        assert_eq!(
            decrypt_token(&other_key, &foreign).unwrap(),
            b"{}",
            "the foreign key itself still decrypts, so the failure above is the key mismatch"
        );

        let (_, public) = keypair();
        let (key, _) = (keypair().0, public.clone());
        let mut ciphertext = URL_SAFE_NO_PAD.decode(encrypt(&public, b"{}")).unwrap();
        ciphertext[0] ^= 0x01;
        assert_eq!(
            decrypt_token(&key, &URL_SAFE_NO_PAD.encode(&ciphertext)).unwrap_err(),
            LoginError::DecryptFailed
        );
        assert_eq!(
            decrypt_token(&key, "not base64!").unwrap_err(),
            LoginError::DecryptFailed
        );
        assert_eq!(
            decrypt_token(&key, &URL_SAFE_NO_PAD.encode([0u8; 255])).unwrap_err(),
            LoginError::DecryptFailed
        );
    }

    #[tokio::test]
    async fn paste_flow_saves_the_account_and_consumes_the_slot() {
        let _guard = global_guard().await;
        std::env::set_var("ZED_LOGIN_PORT", "58443");
        let dir = temp_dir("paste");
        let services = services(&dir);

        let body = start_paste_login(&services, Some("work")).await.unwrap();
        let start: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(start["port"], 58443);
        assert_eq!(start["mode"], "paste");
        assert_eq!(start["expires_in"], 600);
        let url = start["login_url"].as_str().unwrap();
        let prefix =
            "https://zed.dev/native_app_signin?native_app_port=58443&native_app_public_key=";
        assert!(url.starts_with(prefix), "{url}");
        let public = &url[prefix.len()..];
        assert_eq!(take_status(), LoginStatus::Waiting);

        // A second start while active is 409.
        let err = start_paste_login(&services, None).await.unwrap_err();
        assert_eq!(err.status, Some(409));
        assert_eq!(err.message, "login already in progress");

        let token = encrypt(public, br#"{"access_token":"secret"}"#);
        let complete = serde_json::json!({
            "url": format!("http://127.0.0.1:58443/?user_id=42&access_token={token}")
        });

        // A bad callback keeps the slot for a retry (spec 60 §4 rule 5).
        let bad = complete_paste_login(&services, &serde_json::json!({"url": "user_id=42"}))
            .await
            .unwrap_err();
        assert_eq!(bad.status, Some(400));
        assert_eq!(bad.message, "invalid callback url: missing access_token");
        assert!(lock_slot().is_active());

        let done = complete_paste_login(&services, &complete).await.unwrap();
        assert_eq!(
            String::from_utf8(done).unwrap(),
            r#"{"status":"success","name":"work"}"#
        );
        let saved = std::fs::read_to_string(dir.join("accounts.json")).unwrap();
        assert!(saved.contains("\"work\""), "{saved}");
        assert!(saved.contains("\"user_id\":\"42\""), "{saved}");
        assert!(saved.contains("\"access_token\":\"secret\""), "{saved}");
        assert_eq!(
            std::fs::read_to_string(dir.join("active_account.txt")).unwrap(),
            "work"
        );
        // The paste flow answers over HTTP, so the polled status stays idle.
        assert_eq!(take_status(), LoginStatus::Idle);
        // The slot is consumed: a second complete is 409, not 410.
        let again = complete_paste_login(&services, &complete)
            .await
            .unwrap_err();
        assert_eq!(again.status, Some(409));
        assert_eq!(again.message, "no pending login");

        // No private-key material or ciphertext ever reaches the directory.
        let names: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        for name in &names {
            assert!(
                !name.contains("key") && !name.contains("pem") && !name.contains("enc"),
                "{names:?}"
            );
        }
        assert!(
            !names.iter().any(|name| name == ".write_probe"),
            "{names:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
        std::env::remove_var("ZED_LOGIN_PORT");
    }

    #[tokio::test]
    async fn paste_complete_maps_expired_and_absent_slots() {
        let _guard = global_guard().await;
        let dir = temp_dir("expired");
        let services = services(&dir);
        let body = serde_json::json!({"url": "user_id=1&access_token=x"});

        // No flow has ever run: 409, never 410.
        {
            let mut slot = lock_slot();
            *slot = LoginSlot::new();
        }
        let err = complete_paste_login(&services, &body).await.unwrap_err();
        assert_eq!(
            (err.status, err.message.as_str()),
            (Some(409), "no pending login")
        );

        // A flow that expired without being consumed: 410 and the key is gone.
        {
            let mut slot = lock_slot();
            let generation = slot.occupy(100, LOGIN_TTL_SECS).unwrap();
            slot.pending = Some(PendingLogin {
                key: generate_key().unwrap(),
                name: String::new(),
            });
            assert_eq!(slot.generation, generation);
            slot.deadline = 100;
        }
        let err = complete_paste_login(&services, &body).await.unwrap_err();
        assert_eq!(
            (err.status, err.message.as_str()),
            (Some(410), "pending login expired; start again")
        );
        assert!(lock_slot().pending.is_none(), "expired key must be dropped");
        assert_eq!(take_status(), LoginStatus::Failed);

        // Active but with no key: 409 no pending login (spec 60 §5.2 step 3).
        {
            let mut slot = lock_slot();
            let _ = slot.occupy(now_unix(), LOGIN_TTL_SECS).unwrap();
        }
        let err = complete_paste_login(&services, &body).await.unwrap_err();
        assert_eq!(
            (err.status, err.message.as_str()),
            (Some(409), "no pending login")
        );
        {
            let mut slot = lock_slot();
            let generation = slot.generation;
            slot.release(generation);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_account_maps_length_and_json_failures() {
        let dir = temp_dir("save-errors");
        let store = AccountStore::load(&dir, 0);
        let too_long = "x".repeat(MAX_NAME_BYTES + 1);
        let err = save_account(&store, "u", b"{}", Some(&too_long)).unwrap_err();
        assert_eq!(err.status, Some(400));
        assert_eq!(
            err.message,
            "account name or user_id is too long (max 256 bytes)"
        );
        let err = save_account(&store, &too_long, b"{}", None).unwrap_err();
        assert_eq!(err.status, Some(400));
        let err = save_account(&store, "u", b"{not json", None).unwrap_err();
        assert_eq!(err.status, Some(400));
        assert_eq!(err.message, "decrypted credential is not valid JSON");
        assert!(!dir.join("accounts.json").exists(), "nothing was written");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn complete_requires_url_or_query() {
        let dir = temp_dir("missing-url");
        let services = services(&dir);
        for body in [
            serde_json::json!({}),
            serde_json::json!({"url": 5}),
            serde_json::json!("just a string"),
            serde_json::json!([]),
        ] {
            let err = complete_paste_login(&services, &body).await.unwrap_err();
            assert_eq!(
                (err.status, err.message.as_str()),
                (Some(400), "missing url")
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn paste_start_rejects_an_invalid_login_port() {
        let _guard = global_guard().await;
        std::env::set_var("ZED_LOGIN_PORT", "0");
        let dir = temp_dir("bad-port");
        let services = services(&dir);
        let err = start_paste_login(&services, None).await.unwrap_err();
        assert_eq!(err.status, Some(400));
        assert_eq!(err.message, "ZED_LOGIN_PORT must be 1-65535");
        assert!(!lock_slot().is_active(), "a rejected start must not occupy");
        let _ = std::fs::remove_dir_all(&dir);
        std::env::remove_var("ZED_LOGIN_PORT");
    }

    #[test]
    fn local_callback_reads_the_query_and_writes_the_fixed_redirect() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let client = std::thread::spawn(move || {
            let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
            stream
                .write_all(b"GET /?user_id=u1&access_token=tok HTTP/1.1\r\nHost: x\r\n\r\n")
                .unwrap();
            let mut response = String::new();
            stream.read_to_string(&mut response).unwrap();
            response
        });
        let (mut stream, params) = wait_for_callback(&listener, now_unix() + 5).unwrap();
        assert_eq!(params.user_id, "u1");
        assert_eq!(params.access_token, "tok");
        write_redirect(&mut stream);
        drop(stream);
        let response = client.join().unwrap();
        assert_eq!(
            response,
            "HTTP/1.1 302 Found\r\nLocation: https://zed.dev/native_app_signin_succeeded\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        );
    }

    #[test]
    fn local_callback_gives_up_at_the_deadline() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let started = std::time::Instant::now();
        assert!(wait_for_callback(&listener, now_unix()).is_none());
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn data_dir_probe_reports_an_unwritable_directory() {
        let dir = temp_dir("probe");
        assert!(data_dir_writable(&dir));
        assert!(!dir.join(".write_probe").exists());
        // A path that is a file cannot hold the probe.
        let file = dir.join("not-a-dir");
        std::fs::write(&file, b"x").unwrap();
        assert!(!data_dir_writable(&file));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn writable_error_renders_the_two_key_body() {
        let (status, body) = error_response(&writable_error());
        assert_eq!(status, 500);
        let text = String::from_utf8(body).unwrap();
        assert!(text.starts_with(
            r#"{"error":"data directory is not writable","detail":"accounts.json cannot be saved; "#
        ));
        let (status, body) = error_response(&bad_request("missing url"));
        assert_eq!(status, 400);
        assert_eq!(body, br#"{"error":"missing url"}"#.to_vec());
    }
}
