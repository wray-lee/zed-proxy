//! `accounts.json` loading/writing, active account persistence, cooldown state
//! and the per-request attempt order (spec 50 §2–§7, §9).

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use serde::Deserialize;
use serde_json::Value;

use crate::error::{ErrorKind, GatewayError};
use crate::upstream::token::TokenCache;

use super::status::AccountModelState;

/// Account name and `user_id` are limited to 256 bytes (spec 50 §1.1).
pub const MAX_ACCOUNT_FIELD_BYTES: usize = 256;
/// `accounts.json` is capped at 4 MiB (spec 50 §3.1).
pub const MAX_ACCOUNTS_FILE_BYTES: u64 = 4 * 1024 * 1024;
pub const ACCOUNTS_FILE: &str = "accounts.json";
pub const ACTIVE_ACCOUNT_FILE: &str = "active_account.txt";
/// Same-directory temp files used by every atomic write (spec 50 §3.1, §4).
const ACCOUNTS_TEMP_FILE: &str = ".accounts.json.tmp";
const ACTIVE_TEMP_FILE: &str = ".active_account.txt.tmp";

pub const COOLDOWN_AUTH_S: i64 = 300;
pub const COOLDOWN_AUTH_LONG_S: i64 = 3600;
pub const COOLDOWN_AUTH_FAILURES: u32 = 3;
pub const COOLDOWN_RATE_LIMIT_S: i64 = 30;
pub const COOLDOWN_TRANSIENT_S: i64 = 20;
pub const COOLDOWN_TRANSIENT_THRESHOLD: u32 = 3;

/// One account as loaded from disk plus its in-memory state (spec 50 §1.2).
#[derive(Debug, Clone)]
pub struct Account {
    pub name: String,
    pub user_id: String,
    /// Credential JSON, kept compact. Never logged or returned.
    pub credential: Value,
    /// Orb portal link; equivalent to a credential (spec 50 §1.1).
    pub orb_url: Option<String>,
    pub token: TokenCache,
    pub disabled_until: i64,
    pub consecutive_failures: u32,
    pub last_status: u16,
    pub last_ok: i64,
    pub model_state: AccountModelState,
    pub model_checked_at: i64,
    pub model_latency_ms: u64,
}

impl Account {
    pub fn is_healthy(&self, now: i64) -> bool {
        now >= self.disabled_until
    }
}

/// Credential-free copy used by request handlers so they never hold the lock
/// while talking to the network (spec 50 §9 item 2).
#[derive(Debug, Clone)]
pub struct AccountSnapshot {
    pub name: String,
    pub user_id: String,
    pub credential: Value,
    pub orb_url: Option<String>,
    pub disabled_until: i64,
    pub consecutive_failures: u32,
    pub last_status: u16,
    pub model_state: AccountModelState,
    pub model_checked_at: i64,
    pub model_latency_ms: u64,
}

#[derive(Debug)]
pub enum AccountError {
    NotFound,
    FieldTooLong,
    FileTooLarge,
    Io(String),
}

impl AccountError {
    /// The short, credential-free string used in `failed to save account: <…>`
    /// (spec 70 §3.19).
    pub fn short_name(&self) -> &'static str {
        match self {
            AccountError::NotFound => "not_found",
            AccountError::FieldTooLong => "field_too_long",
            AccountError::FileTooLarge => "file_too_large",
            AccountError::Io(_) => "io_error",
        }
    }
}

impl fmt::Display for AccountError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AccountError::NotFound => write!(f, "account not found"),
            AccountError::FieldTooLong => write!(f, "field too long"),
            AccountError::FileTooLarge => write!(f, "accounts file too large"),
            AccountError::Io(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for AccountError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AddAccountOutcome {
    Added,
    Replaced,
}

/// The pool. All mutation happens under one lock; the file write path uses a
/// second lock so read-modify-rename is atomic with respect to other writers
/// (spec 50 §3.3, §9).
pub struct AccountStore {
    accounts: Mutex<Vec<Account>>,
    current: Mutex<usize>,
    write_lock: Mutex<()>,
    data_dir: PathBuf,
}

impl fmt::Debug for AccountStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AccountStore")
            .field("data_dir", &self.data_dir)
            .finish_non_exhaustive()
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // A panic elsewhere must not turn the pool into a permanent 500: the
    // guarded data is plain values, so recovering the guard is safe.
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

// ---------------------------------------------------------------------------
// On-disk document: order-preserving parse and atomic write (spec 50 §2–§3)
// ---------------------------------------------------------------------------

fn warn(message: &str) {
    eprintln!("[zed-proxy] {message}");
}

/// One account object exactly as it appeared on disk, key order preserved so a
/// rewrite never reorders or drops what it did not touch (spec 50 §3.1).
type OrderedAccount = Vec<(String, Value)>;

#[derive(Debug, Default)]
struct AccountsDocument {
    accounts: Vec<(String, OrderedAccount)>,
}

impl<'de> serde::Deserialize<'de> for AccountsDocument {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // The root must be an object; `#[serde(deny_unknown_fields)]` is not
        // wanted (unknown root keys are ignored, spec 50 §2.2).
        #[derive(Deserialize)]
        struct Root {
            #[serde(default)]
            accounts: AccountsMap,
        }
        let root = Root::deserialize(deserializer)?;
        Ok(AccountsDocument {
            accounts: root.accounts.0,
        })
    }
}

/// `accounts` map; a non-object value yields an empty map instead of failing
/// the whole file (spec 50 §2.2: `{"accounts":5}` loads 0 accounts).
#[derive(Debug, Default)]
struct AccountsMap(Vec<(String, OrderedAccount)>);

impl<'de> serde::Deserialize<'de> for AccountsMap {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(AccountsMapVisitor)
    }
}

struct AccountsMapVisitor;

impl<'de> serde::de::Visitor<'de> for AccountsMapVisitor {
    type Value = AccountsMap;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("an object of accounts")
    }

    fn visit_map<M: serde::de::MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
        let mut entries = Vec::new();
        while let Some(name) = map.next_key::<String>()? {
            let account: LenientAccount = map.next_value()?;
            if let Some(fields) = account.0 {
                entries.push((name, fields));
            }
        }
        Ok(AccountsMap(entries))
    }

    // Arrays, scalars and null are all "no accounts" (spec 50 §2.2).
    fn visit_bool<E>(self, _: bool) -> Result<Self::Value, E> {
        Ok(AccountsMap::default())
    }
    fn visit_i64<E>(self, _: i64) -> Result<Self::Value, E> {
        Ok(AccountsMap::default())
    }
    fn visit_u64<E>(self, _: u64) -> Result<Self::Value, E> {
        Ok(AccountsMap::default())
    }
    fn visit_f64<E>(self, _: f64) -> Result<Self::Value, E> {
        Ok(AccountsMap::default())
    }
    fn visit_str<E>(self, _: &str) -> Result<Self::Value, E> {
        Ok(AccountsMap::default())
    }
    fn visit_string<E>(self, _: String) -> Result<Self::Value, E> {
        Ok(AccountsMap::default())
    }
    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(AccountsMap::default())
    }
    fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        while seq.next_element::<serde::de::IgnoredAny>()?.is_some() {}
        Ok(AccountsMap::default())
    }
}

/// One account value: `Some(fields)` when it is an object, `None` otherwise
/// (that entry is skipped, the rest of the file keeps loading, spec 50 §2.2).
struct LenientAccount(Option<OrderedAccount>);

impl<'de> serde::Deserialize<'de> for LenientAccount {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(LenientAccountVisitor)
    }
}

struct LenientAccountVisitor;

impl<'de> serde::de::Visitor<'de> for LenientAccountVisitor {
    type Value = LenientAccount;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("an account object")
    }

    fn visit_map<M: serde::de::MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
        let mut fields = Vec::new();
        while let Some(key) = map.next_key::<String>()? {
            let value: Value = map.next_value()?;
            fields.push((key, value));
        }
        Ok(LenientAccount(Some(fields)))
    }

    fn visit_bool<E>(self, _: bool) -> Result<Self::Value, E> {
        Ok(LenientAccount(None))
    }
    fn visit_i64<E>(self, _: i64) -> Result<Self::Value, E> {
        Ok(LenientAccount(None))
    }
    fn visit_u64<E>(self, _: u64) -> Result<Self::Value, E> {
        Ok(LenientAccount(None))
    }
    fn visit_f64<E>(self, _: f64) -> Result<Self::Value, E> {
        Ok(LenientAccount(None))
    }
    fn visit_str<E>(self, _: &str) -> Result<Self::Value, E> {
        Ok(LenientAccount(None))
    }
    fn visit_string<E>(self, _: String) -> Result<Self::Value, E> {
        Ok(LenientAccount(None))
    }
    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(LenientAccount(None))
    }
    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(LenientAccount(None))
    }
    fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        while seq.next_element::<serde::de::IgnoredAny>()?.is_some() {}
        Ok(LenientAccount(None))
    }
}

/// Strict document for the write path: the root must be an object and, when
/// present, `accounts` must be an object. Anything else is refused so a save
/// never replaces data this code cannot understand (spec 50 §3.2).
fn parse_strict(bytes: &[u8]) -> Result<AccountsDocument, AccountError> {
    let root: Value = serde_json::from_slice(bytes)
        .map_err(|_| AccountError::Io("accounts file is not valid JSON".to_string()))?;
    let Value::Object(mut root) = root else {
        return Err(AccountError::Io(
            "accounts file root is not an object".to_string(),
        ));
    };
    match root.remove("accounts") {
        None | Some(Value::Null) => Ok(AccountsDocument::default()),
        Some(Value::Object(map)) => Ok(AccountsDocument {
            accounts: map
                .into_iter()
                .filter_map(|(name, value)| match value {
                    Value::Object(fields) => Some((name, fields.into_iter().collect())),
                    _ => None,
                })
                .collect(),
        }),
        Some(_) => Err(AccountError::Io(
            "accounts file has no accounts object".to_string(),
        )),
    }
}

/// Read `accounts.json` without ever failing: every malformed input yields an
/// empty document plus one log line (spec 50 §2.2).
fn read_document(path: &Path) -> AccountsDocument {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return AccountsDocument::default(),
        Err(err) => {
            warn(&format!(
                "accounts.json could not be read ({err}); starting with 0 accounts"
            ));
            return AccountsDocument::default();
        }
    };
    if metadata.len() > MAX_ACCOUNTS_FILE_BYTES {
        warn("accounts.json exceeds 4 MiB; starting with 0 accounts");
        return AccountsDocument::default();
    }
    let mut bytes = Vec::new();
    match File::open(path).and_then(|file| {
        file.take(MAX_ACCOUNTS_FILE_BYTES + 1)
            .read_to_end(&mut bytes)
    }) {
        Ok(_) if bytes.len() as u64 > MAX_ACCOUNTS_FILE_BYTES => {
            warn("accounts.json exceeds 4 MiB; starting with 0 accounts");
            return AccountsDocument::default();
        }
        Ok(_) => {}
        Err(err) => {
            warn(&format!(
                "accounts.json could not be read ({err}); starting with 0 accounts"
            ));
            return AccountsDocument::default();
        }
    }
    match serde_json::from_slice::<AccountsDocument>(&bytes) {
        Ok(document) => document,
        Err(_) => {
            warn("accounts.json is not valid JSON; starting with 0 accounts");
            AccountsDocument::default()
        }
    }
}

/// Strict read for the write paths: "missing" is an empty document, but a
/// malformed file is refused so stored accounts are never overwritten.
fn read_document_for_write(path: &Path) -> Result<AccountsDocument, AccountError> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(AccountsDocument::default()),
        Err(err) => {
            return Err(AccountError::Io(format!(
                "cannot read accounts file: {err}"
            )))
        }
    };
    if bytes.len() as u64 > MAX_ACCOUNTS_FILE_BYTES {
        return Err(AccountError::FileTooLarge);
    }
    parse_strict(&bytes)
}

fn field_get<'a>(fields: &'a OrderedAccount, key: &str) -> Option<&'a Value> {
    fields
        .iter()
        .find(|(name, _)| name == key)
        .map(|(_, value)| value)
}

fn field_set(fields: &mut OrderedAccount, key: &str, value: Value) {
    match fields.iter_mut().find(|(name, _)| name == key) {
        Some((_, slot)) => *slot = value,
        None => fields.push((key.to_string(), value)),
    }
}

fn field_remove(fields: &mut OrderedAccount, key: &str) {
    fields.retain(|(name, _)| name != key);
}

/// `user_id` accepts a string or an integer rendered in decimal (spec 50 §2.2).
fn user_id_string(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) if number.is_i64() || number.is_u64() => Some(number.to_string()),
        _ => None,
    }
}

fn orb_url_from(fields: &OrderedAccount) -> Option<String> {
    field_get(fields, "orb_url")
        .and_then(Value::as_str)
        .filter(|url| !url.is_empty())
        .map(str::to_string)
}

/// Build an in-memory account from its on-disk fields; `None` when the entry is
/// unusable (no `user_id` or no `credential`, spec 50 §2.2).
fn account_from(fields: &OrderedAccount, name: &str) -> Option<Account> {
    let user_id = user_id_string(field_get(fields, "user_id")?)?;
    let credential = compact(field_get(fields, "credential")?);
    Some(Account {
        name: name.to_string(),
        user_id,
        credential,
        orb_url: orb_url_from(fields),
        token: TokenCache::new(),
        disabled_until: 0,
        consecutive_failures: 0,
        last_status: 0,
        last_ok: 0,
        model_state: AccountModelState::Unchecked,
        model_checked_at: 0,
        model_latency_ms: 0,
    })
}

/// Credentials are held in compact form (spec 50 §2.2); parsing cannot fail
/// because the value came from `serde_json`.
fn compact(value: &Value) -> Value {
    match serde_json::to_string(value) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_else(|_| value.clone()),
        Err(_) => value.clone(),
    }
}

/// Two-space-indented document, keys in the order they were loaded, with
/// `name`/`user_id` unique (spec 50 §3.1).
fn render_document(entries: &[(String, OrderedAccount)]) -> String {
    let mut out = String::from("{\n  \"accounts\": {");
    for (index, (name, fields)) in entries.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        out.push_str("\n    ");
        push_quoted(&mut out, name);
        out.push_str(": {");
        // Fixed field order (spec 50 §3.1): user_id, credential, orb_url. The
        // parser does not preserve order, so it is re-imposed here.
        let mut emitted = 0;
        for key in ["user_id", "credential", "orb_url"] {
            let Some(value) = field_get(fields, key) else {
                continue;
            };
            if emitted > 0 {
                out.push(',');
            }
            push_quoted(&mut out, key);
            out.push(':');
            push_value(&mut out, value);
            emitted += 1;
        }
        for (key, value) in fields.iter() {
            if matches!(key.as_str(), "user_id" | "credential" | "orb_url") {
                continue;
            }
            if emitted > 0 {
                out.push(',');
            }
            push_quoted(&mut out, key);
            out.push(':');
            push_value(&mut out, value);
            emitted += 1;
        }
        out.push('}');
    }
    if entries.is_empty() {
        out.push_str("}\n}\n");
    } else {
        out.push_str("\n  }\n}\n");
    }
    out
}

fn push_quoted(out: &mut String, value: &str) {
    // serde_json always escapes to a valid JSON string; fall back to `""`.
    match serde_json::to_string(value) {
        Ok(text) => out.push_str(&text),
        Err(_) => out.push_str("\"\""),
    }
}

/// Compact value, but keep an object's key order (serde_json's `Value` would
/// sort keys; the file's own order must survive a rewrite, spec 50 §3.1).
fn push_value(out: &mut String, value: &Value) {
    match value {
        Value::Object(map) => {
            out.push('{');
            for (index, (key, item)) in map.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                push_quoted(out, key);
                out.push(':');
                push_value(out, item);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                push_value(out, item);
            }
            out.push(']');
        }
        other => match serde_json::to_string(other) {
            Ok(text) => out.push_str(&text),
            Err(_) => out.push_str("null"),
        },
    }
}

fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

/// Write `path` atomically: same-directory 0600 temp file, then `rename`.
/// On any failure the temp file is removed and the original stays untouched
/// (spec 50 §3.1 item 6).
fn atomic_write(dir: &Path, temp_name: &str, final_name: &str, bytes: &[u8]) -> io::Result<()> {
    let temp = dir.join(temp_name);
    let final_path = dir.join(final_name);
    let result = write_private(&temp, bytes).and_then(|()| fs::rename(&temp, &final_path));
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

fn write_active_account(dir: &Path, name: &str) {
    // Best effort (spec 50 §4): a failure never affects the request.
    let _ = atomic_write(dir, ACTIVE_TEMP_FILE, ACTIVE_ACCOUNT_FILE, name.as_bytes());
}

/// Remembered active account, trimmed and capped at 256 bytes (spec 50 §4).
fn read_active_account(dir: &Path) -> Option<String> {
    let mut bytes = Vec::new();
    File::open(dir.join(ACTIVE_ACCOUNT_FILE))
        .ok()?
        .take(MAX_ACCOUNT_FIELD_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() > MAX_ACCOUNT_FIELD_BYTES {
        return None;
    }
    let name = String::from_utf8(bytes).ok()?.trim().to_string();
    (!name.is_empty()).then_some(name)
}

fn field_len(value: &str) -> usize {
    value.len()
}

impl AccountStore {
    /// Load `accounts.json` from `data_dir`, tolerating every malformed input
    /// (spec 50 §2.2) and restoring the active account (spec 50 §4).
    pub fn load(data_dir: impl Into<PathBuf>, now: i64) -> Self {
        let data_dir = data_dir.into();
        let document = read_document(&data_dir.join(ACCOUNTS_FILE));
        let mut accounts: Vec<Account> = Vec::with_capacity(document.accounts.len());
        for (name, fields) in &document.accounts {
            match account_from(fields, name) {
                Some(account) => accounts.push(account),
                None => warn(&format!(
                    "account '{name}' skipped: missing or invalid user_id/credential"
                )),
            }
        }
        let current = initial_current(&accounts, read_active_account(&data_dir), now);
        Self {
            accounts: Mutex::new(accounts),
            current: Mutex::new(current),
            write_lock: Mutex::new(()),
            data_dir,
        }
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// Reload the file after a login (spec 50 §9 item 2). Cooldown and probe
    /// state are carried over by account name; the current account follows its
    /// name, else index 0.
    pub fn reload(&self) -> Result<(), AccountError> {
        let _write_guard = lock(&self.write_lock);
        let document = read_document_for_write(&self.data_dir.join(ACCOUNTS_FILE))?;
        let previous_name = self.current_name();
        let mut accounts = lock(&self.accounts);
        let mut next: Vec<Account> = Vec::with_capacity(document.accounts.len());
        for (name, fields) in &document.accounts {
            let Some(mut account) = account_from(fields, name) else {
                warn(&format!(
                    "account '{name}' skipped: missing or invalid user_id/credential"
                ));
                continue;
            };
            if let Some(old) = accounts.iter().find(|old| old.name == account.name) {
                account.token = old.token.clone();
                account.disabled_until = old.disabled_until;
                account.consecutive_failures = old.consecutive_failures;
                account.last_status = old.last_status;
                account.last_ok = old.last_ok;
                account.model_state = old.model_state;
                account.model_checked_at = old.model_checked_at;
                account.model_latency_ms = old.model_latency_ms;
            }
            next.push(account);
        }
        let next_current = next
            .iter()
            .position(|account| account.name == previous_name)
            .unwrap_or(0);
        *accounts = next;
        *lock(&self.current) = next_current;
        Ok(())
    }

    pub fn len(&self) -> usize {
        lock(&self.accounts).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn names(&self) -> Vec<String> {
        lock(&self.accounts)
            .iter()
            .map(|account| account.name.clone())
            .collect()
    }

    pub fn snapshots(&self) -> Vec<AccountSnapshot> {
        lock(&self.accounts).iter().map(snapshot_of).collect()
    }

    pub fn snapshot(&self, name: &str) -> Option<AccountSnapshot> {
        lock(&self.accounts)
            .iter()
            .find(|account| account.name == name)
            .map(snapshot_of)
    }

    pub fn current_name(&self) -> String {
        let current = *lock(&self.current);
        lock(&self.accounts)
            .get(current)
            .map(|account| account.name.clone())
            .unwrap_or_default()
    }

    /// Attempt order for one request (spec 50 §6): current account first when
    /// healthy, then other healthy accounts in file order, then cooling ones by
    /// earliest recovery. Every account appears exactly once, no size limit.
    pub fn attempt_order(&self, now: i64) -> Vec<String> {
        let accounts = lock(&self.accounts);
        let current = *lock(&self.current);
        let mut order: Vec<usize> = Vec::with_capacity(accounts.len());
        if let Some(account) = accounts.get(current) {
            if account.is_healthy(now) {
                order.push(current);
            }
        }
        for (index, account) in accounts.iter().enumerate() {
            if index != current && account.is_healthy(now) {
                order.push(index);
            }
        }
        let mut cooling: Vec<usize> = (0..accounts.len())
            .filter(|index| !accounts[*index].is_healthy(now))
            .collect();
        // Stable by index, so equal `disabled_until` keeps file order.
        cooling.sort_by_key(|index| accounts[*index].disabled_until);
        order.extend(cooling);
        order
            .into_iter()
            .filter_map(|index| accounts.get(index).map(|account| account.name.clone()))
            .collect()
    }

    pub fn switch(&self, name: &str) -> Result<(), AccountError> {
        let index = {
            let accounts = lock(&self.accounts);
            accounts
                .iter()
                .position(|account| account.name == name)
                .ok_or(AccountError::NotFound)?
        };
        *lock(&self.current) = index;
        write_active_account(&self.data_dir, name);
        Ok(())
    }

    /// Promote `name` to current after a failover success (spec 50 §7.1 item 3).
    /// A missing name is a no-op.
    pub fn set_current(&self, name: &str) {
        let _ = self.switch(name);
    }

    /// Record a success (spec 50 §5.2).
    pub fn record_success(&self, name: &str, now: i64) {
        let mut accounts = lock(&self.accounts);
        if let Some(account) = accounts.iter_mut().find(|account| account.name == name) {
            account.consecutive_failures = 0;
            account.disabled_until = 0;
            account.last_status = 200;
            account.last_ok = now;
        }
    }

    /// Record an account-level failure and compute the cooldown (spec 50 §5.3).
    /// Request-level and plan-level failures must not call this.
    pub fn record_failure(&self, name: &str, kind: ErrorKind, status: u16, now: i64) {
        if !kind.is_account_level() {
            return;
        }
        let mut accounts = lock(&self.accounts);
        if let Some(account) = accounts.iter_mut().find(|account| account.name == name) {
            account.consecutive_failures = account.consecutive_failures.saturating_add(1);
            account.last_status = status;
            let cooldown = Self::cooldown_for(kind, account.consecutive_failures);
            // Zero cooldown means "not cooled"; keeping `now` would leave a
            // stale deadline that the health check has to mask.
            account.disabled_until = if cooldown == 0 {
                0
            } else {
                now.saturating_add(cooldown)
            };
        }
    }

    /// Cooldown duration for a failure kind at the new consecutive-failure
    /// count (spec 50 §5.3). Pure, so the table is testable.
    pub fn cooldown_for(kind: ErrorKind, consecutive_failures: u32) -> i64 {
        match kind {
            ErrorKind::Auth => {
                if consecutive_failures >= COOLDOWN_AUTH_FAILURES {
                    COOLDOWN_AUTH_LONG_S
                } else {
                    COOLDOWN_AUTH_S
                }
            }
            ErrorKind::RateLimit => COOLDOWN_RATE_LIMIT_S,
            ErrorKind::Transient => {
                if consecutive_failures >= COOLDOWN_TRANSIENT_THRESHOLD {
                    COOLDOWN_TRANSIENT_S
                } else {
                    0
                }
            }
            ErrorKind::Request | ErrorKind::Plan | ErrorKind::ClientDisconnect => 0,
        }
    }

    /// Insert or replace an account and persist atomically (spec 50 §3.1).
    pub fn add_account(
        &self,
        name: &str,
        user_id: &str,
        credential: Value,
    ) -> Result<AddAccountOutcome, AccountError> {
        if field_len(name) > MAX_ACCOUNT_FIELD_BYTES || field_len(user_id) > MAX_ACCOUNT_FIELD_BYTES
        {
            return Err(AccountError::FieldTooLong);
        }
        let credential = compact(&credential);
        let _write_guard = lock(&self.write_lock);
        let path = self.data_dir.join(ACCOUNTS_FILE);
        let mut document = read_document_for_write(&path)?;
        let mut fields: OrderedAccount = vec![
            ("user_id".to_string(), Value::String(user_id.to_string())),
            ("credential".to_string(), credential.clone()),
        ];
        let mut outcome = AddAccountOutcome::Added;
        let mut kept: Vec<(String, OrderedAccount)> =
            Vec::with_capacity(document.accounts.len() + 1);
        for (old_name, mut old_fields) in document.accounts.drain(..) {
            let same_user =
                user_id_string(field_get(&old_fields, "user_id").unwrap_or(&Value::Null))
                    .is_some_and(|old| old == user_id);
            if old_name == name || same_user {
                outcome = AddAccountOutcome::Replaced;
                // The surviving entry keeps its stored `orb_url` (spec 50 §3.1).
                if let Some(url) = orb_url_from(&old_fields) {
                    fields.push(("orb_url".to_string(), Value::String(url)));
                }
                continue;
            }
            keep_declared_fields(&mut old_fields);
            if !old_fields.is_empty() {
                kept.push((old_name, old_fields));
            }
        }
        kept.push((name.to_string(), fields));
        write_accounts(&self.data_dir, &kept)?;
        self.apply_account(name, user_id, credential);
        Ok(outcome)
    }

    /// Set/clear `orb_url` for an account (spec 50 §3.2).
    pub fn set_orb_url(&self, name: &str, url: Option<&str>) -> Result<(), AccountError> {
        if self.snapshot(name).is_none() {
            return Err(AccountError::NotFound);
        }
        let _write_guard = lock(&self.write_lock);
        let path = self.data_dir.join(ACCOUNTS_FILE);
        let mut document = read_document_for_write(&path)?;
        let mut found = false;
        let mut kept: Vec<(String, OrderedAccount)> = Vec::with_capacity(document.accounts.len());
        for (old_name, mut fields) in document.accounts.drain(..) {
            if old_name == name {
                found = true;
                match url {
                    Some(value) => {
                        field_set(&mut fields, "orb_url", Value::String(value.to_string()))
                    }
                    None => field_remove(&mut fields, "orb_url"),
                }
            }
            kept.push((old_name, fields));
        }
        if !found {
            return Err(AccountError::NotFound);
        }
        write_accounts(&self.data_dir, &kept)?;
        if let Some(account) = lock(&self.accounts)
            .iter_mut()
            .find(|account| account.name == name)
        {
            account.orb_url = url.map(str::to_string);
        }
        Ok(())
    }

    pub fn set_model_probe(
        &self,
        name: &str,
        state: AccountModelState,
        checked_at: i64,
        latency_ms: u64,
    ) {
        if let Some(account) = lock(&self.accounts)
            .iter_mut()
            .find(|account| account.name == name)
        {
            account.model_state = state;
            account.model_checked_at = checked_at;
            account.model_latency_ms = latency_ms;
        }
    }

    /// Map an account error to the HTTP-facing message (spec 70 §3.19).
    pub fn save_error_body(err: &AccountError) -> GatewayError {
        let mut gateway = GatewayError::new(
            ErrorKind::Transient,
            format!("failed to save account: {}", err.short_name()),
        );
        gateway.code = Some("failed_to_save_account");
        gateway
    }

    fn apply_account(&self, name: &str, user_id: &str, credential: Value) {
        let mut accounts = lock(&self.accounts);
        match accounts.iter_mut().find(|account| account.name == name) {
            Some(account) => {
                account.user_id = user_id.to_string();
                account.credential = credential;
            }
            None => {
                // Same `user_id` under a new name replaces the old entry.
                accounts.retain(|account| account.user_id != user_id);
                accounts.push(Account {
                    name: name.to_string(),
                    user_id: user_id.to_string(),
                    credential,
                    orb_url: None,
                    token: TokenCache::new(),
                    disabled_until: 0,
                    consecutive_failures: 0,
                    last_status: 0,
                    last_ok: 0,
                    model_state: AccountModelState::Unchecked,
                    model_checked_at: 0,
                    model_latency_ms: 0,
                });
            }
        }
    }
}

/// Entries preserved from an existing file keep only the fields this store
/// understands; anything else cannot be validated and is dropped rather than
/// copied blindly into a new file.
fn keep_declared_fields(fields: &mut OrderedAccount) {
    fields.retain(|(key, _)| matches!(key.as_str(), "user_id" | "credential" | "orb_url"));
    if field_get(fields, "user_id").is_none() || field_get(fields, "credential").is_none() {
        fields.clear();
    }
}

fn write_accounts(dir: &Path, entries: &[(String, OrderedAccount)]) -> Result<(), AccountError> {
    let text = render_document(entries);
    atomic_write(dir, ACCOUNTS_TEMP_FILE, ACCOUNTS_FILE, text.as_bytes())
        .map_err(|err| AccountError::Io(format!("cannot write accounts file: {err}")))
}

fn snapshot_of(account: &Account) -> AccountSnapshot {
    AccountSnapshot {
        name: account.name.clone(),
        user_id: account.user_id.clone(),
        credential: account.credential.clone(),
        orb_url: account.orb_url.clone(),
        disabled_until: account.disabled_until,
        consecutive_failures: account.consecutive_failures,
        last_status: account.last_status,
        model_state: account.model_state,
        model_checked_at: account.model_checked_at,
        model_latency_ms: account.model_latency_ms,
    }
}

/// Startup selection (spec 50 §4): remembered name, else the first healthy
/// account, else index 0.
fn initial_current(accounts: &[Account], remembered: Option<String>, now: i64) -> usize {
    if let Some(name) = remembered {
        if let Some(index) = accounts.iter().position(|account| account.name == name) {
            return index;
        }
    }
    accounts
        .iter()
        .position(|account| account.is_healthy(now))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "zprs-accounts-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn add(store: &AccountStore, name: &str, uid: &str) {
        let credential = serde_json::json!({"access_token": format!("token-{name}")});
        store.add_account(name, uid, credential).unwrap();
    }

    #[test]
    fn cooldown_table_matches_the_spec() {
        assert_eq!(AccountStore::cooldown_for(ErrorKind::Auth, 1), 300);
        assert_eq!(AccountStore::cooldown_for(ErrorKind::Auth, 2), 300);
        assert_eq!(AccountStore::cooldown_for(ErrorKind::Auth, 3), 3600);
        assert_eq!(AccountStore::cooldown_for(ErrorKind::RateLimit, 1), 30);
        assert_eq!(AccountStore::cooldown_for(ErrorKind::RateLimit, 9), 30);
        assert_eq!(AccountStore::cooldown_for(ErrorKind::Transient, 1), 0);
        assert_eq!(AccountStore::cooldown_for(ErrorKind::Transient, 2), 0);
        assert_eq!(AccountStore::cooldown_for(ErrorKind::Transient, 3), 20);
        assert_eq!(AccountStore::cooldown_for(ErrorKind::Request, 9), 0);
        assert_eq!(AccountStore::cooldown_for(ErrorKind::Plan, 9), 0);
        assert_eq!(
            AccountStore::cooldown_for(ErrorKind::ClientDisconnect, 9),
            0
        );
    }

    #[test]
    fn auth_and_transient_share_one_counter() {
        // Two credential failures then one transient failure: the transient
        // branch sees 3 and benches the account (spec 50 §5.1).
        assert_eq!(AccountStore::cooldown_for(ErrorKind::Transient, 3), 20);
        assert_eq!(AccountStore::cooldown_for(ErrorKind::Auth, 3), 3600);
    }

    #[test]
    fn names_and_user_ids_survive_json_round_trip_and_appends_keep_old_entries() {
        let dir = temp_dir("escape");
        let store = AccountStore::load(&dir, 0);
        let tricky = "a\"b\\c\td\ne";
        add(&store, tricky, "42");
        add(&store, "second", "7");

        let reloaded = AccountStore::load(&dir, 0);
        assert_eq!(reloaded.len(), 2);
        assert_eq!(
            reloaded.names(),
            vec![tricky.to_string(), "second".to_string()]
        );
        assert_eq!(reloaded.snapshot(tricky).unwrap().user_id, "42");
        // The raw file must be plain JSON with escaped control characters.
        let raw = fs::read_to_string(dir.join(ACCOUNTS_FILE)).unwrap();
        assert!(raw.contains("\\t"), "{raw}");
        assert!(raw.contains("\\n"), "{raw}");
        assert!(serde_json::from_str::<Value>(&raw).unwrap().is_object());

        // Field order is fixed (spec 50 §3.1).
        let first = raw.find("user_id").unwrap();
        let second = raw.find("credential").unwrap();
        assert!(first < second, "{raw}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn integer_user_id_is_loaded_as_decimal_text() {
        let dir = temp_dir("int-uid");
        fs::write(
            dir.join(ACCOUNTS_FILE),
            r#"{"accounts":{"a":{"user_id":42,"credential":{}}}}"#,
        )
        .unwrap();
        let store = AccountStore::load(&dir, 0);
        assert_eq!(store.snapshot("a").unwrap().user_id, "42");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn malformed_documents_load_as_zero_accounts_without_panicking() {
        let cases = [
            "[]",
            "5",
            "\"x\"",
            "null",
            "{\"accounts\":5}",
            "{\"accounts\":[]}",
            "{",
            "{\"accounts\":{\"a\":5,\"b\":[],\"c\":null}}",
            "{\"accounts\":{\"a\":{\"credential\":{}},\"b\":{\"user_id\":\"1\"}}}",
        ];
        for (index, case) in cases.iter().enumerate() {
            let dir = temp_dir(&format!("bad-{index}"));
            fs::write(dir.join(ACCOUNTS_FILE), case).unwrap();
            let store = AccountStore::load(&dir, 0);
            assert_eq!(store.len(), 0, "{case}");
            let _ = fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn oversized_file_is_ignored() {
        let dir = temp_dir("big");
        let mut text = String::from("{\"accounts\":{");
        while text.len() as u64 <= MAX_ACCOUNTS_FILE_BYTES {
            text.push_str("\"pad\":{},");
        }
        text.push_str("\"a\":{\"user_id\":\"1\",\"credential\":{}}}}");
        fs::write(dir.join(ACCOUNTS_FILE), text).unwrap();
        assert_eq!(AccountStore::load(&dir, 0).len(), 0);
        let _ = fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod tests_extra {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "zprs-accounts-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn add(store: &AccountStore, name: &str, uid: &str) {
        let credential = serde_json::json!({"access_token": format!("token-{name}")});
        store.add_account(name, uid, credential).unwrap();
    }

    #[test]
    fn field_limits_are_rejected_without_truncation() {
        let dir = temp_dir("limits");
        let store = AccountStore::load(&dir, 0);
        let long = "x".repeat(MAX_ACCOUNT_FIELD_BYTES + 1);
        assert!(matches!(
            store.add_account(&long, "1", serde_json::json!({})),
            Err(AccountError::FieldTooLong)
        ));
        assert!(matches!(
            store.add_account("a", &long, serde_json::json!({})),
            Err(AccountError::FieldTooLong)
        ));
        let exactly = "x".repeat(MAX_ACCOUNT_FIELD_BYTES);
        assert!(store
            .add_account(&exactly, "1", serde_json::json!({}))
            .is_ok());
        assert_eq!(AccountStore::load(&dir, 0).len(), 1);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn add_account_replaces_by_name_or_user_id_and_keeps_the_orb_link() {
        let dir = temp_dir("replace");
        let store = AccountStore::load(&dir, 0);
        add(&store, "alice", "1");
        store
            .set_orb_url("alice", Some("https://portal.withorb.com/?token=t"))
            .unwrap();
        assert_eq!(
            store
                .add_account("alice", "2", serde_json::json!({"k": 1}))
                .unwrap(),
            AddAccountOutcome::Replaced
        );
        let reloaded = AccountStore::load(&dir, 0);
        assert_eq!(reloaded.len(), 1);
        assert_eq!(reloaded.snapshot("alice").unwrap().user_id, "2");
        assert_eq!(
            reloaded.snapshot("alice").unwrap().orb_url.as_deref(),
            Some("https://portal.withorb.com/?token=t")
        );
        // Same user_id under a new name replaces the old entry too.
        assert_eq!(
            store
                .add_account("bob", "2", serde_json::json!({}))
                .unwrap(),
            AddAccountOutcome::Replaced
        );
        assert_eq!(AccountStore::load(&dir, 0).names(), vec!["bob".to_string()]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn orb_url_round_trips_and_can_be_cleared() {
        let dir = temp_dir("orb");
        let store = AccountStore::load(&dir, 0);
        add(&store, "alice", "1");
        let link = "https://portal.withorb.com/?x=1&token=abc";
        store.set_orb_url("alice", Some(link)).unwrap();
        assert_eq!(
            AccountStore::load(&dir, 0)
                .snapshot("alice")
                .unwrap()
                .orb_url
                .as_deref(),
            Some(link)
        );
        // A login rewrite keeps the link.
        store
            .add_account("alice", "1", serde_json::json!({"access_token": "new"}))
            .unwrap();
        let account = AccountStore::load(&dir, 0).snapshot("alice").unwrap();
        assert_eq!(account.orb_url.as_deref(), Some(link));
        assert_eq!(
            account.credential,
            serde_json::json!({"access_token": "new"})
        );
        // Clearing removes only the link.
        store.set_orb_url("alice", None).unwrap();
        let account = AccountStore::load(&dir, 0).snapshot("alice").unwrap();
        assert!(account.orb_url.is_none());
        assert_eq!(
            account.credential,
            serde_json::json!({"access_token": "new"})
        );
        assert!(matches!(
            store.set_orb_url("nobody", Some(link)),
            Err(AccountError::NotFound)
        ));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn unreadable_existing_file_is_not_overwritten_by_a_save() {
        let dir = temp_dir("atomic");
        let path = dir.join(ACCOUNTS_FILE);
        let original = "{\"accounts\":\"not-an-object\"}";
        fs::write(&path, original).unwrap();
        let store = AccountStore::load(&dir, 0);
        // The load path tolerates it, but a save must not silently replace a
        // file it cannot parse.
        assert!(store.add_account("a", "1", serde_json::json!({})).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
        let leftovers: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .filter(|name| name.contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_failure_leaves_the_old_file_and_no_temp_file() {
        let dir = temp_dir("fail-write");
        let store = AccountStore::load(&dir, 0);
        add(&store, "alice", "1");
        let target = dir.join(ACCOUNTS_FILE);
        let before = fs::read_to_string(&target).unwrap();
        // A directory where the temp file must go makes the write fail.
        fs::create_dir(dir.join(".accounts.json.tmp")).unwrap();
        assert!(store
            .add_account("bob", "2", serde_json::json!({}))
            .is_err());
        assert_eq!(fs::read_to_string(&target).unwrap(), before);
        fs::remove_dir(dir.join(".accounts.json.tmp")).unwrap();
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn active_account_is_restored_and_written_with_the_switch() {
        let dir = temp_dir("active");
        let store = AccountStore::load(&dir, 0);
        add(&store, "alice", "1");
        add(&store, "bob", "2");
        assert_eq!(store.current_name(), "alice");
        store.switch("bob").unwrap();
        assert_eq!(store.current_name(), "bob");
        assert_eq!(
            fs::read_to_string(dir.join(ACTIVE_ACCOUNT_FILE)).unwrap(),
            "bob"
        );
        assert_eq!(AccountStore::load(&dir, 0).current_name(), "bob");
        // A forgotten name falls back to the first healthy account.
        fs::write(dir.join(ACTIVE_ACCOUNT_FILE), "ghost\n").unwrap();
        assert_eq!(AccountStore::load(&dir, 0).current_name(), "alice");
        // Whitespace-only counts as no match.
        fs::write(dir.join(ACTIVE_ACCOUNT_FILE), "  \n").unwrap();
        assert_eq!(AccountStore::load(&dir, 0).current_name(), "alice");
        assert!(matches!(store.switch("ghost"), Err(AccountError::NotFound)));
        let _ = fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod tests_order {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "zprs-accounts-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn add(store: &AccountStore, name: &str, uid: &str) {
        let credential = serde_json::json!({"access_token": format!("token-{name}")});
        store.add_account(name, uid, credential).unwrap();
    }

    #[test]
    fn attempt_order_puts_current_first_then_healthy_then_earliest_recovery() {
        let dir = temp_dir("order");
        let store = AccountStore::load(&dir, 0);
        for (name, uid) in [("a", "1"), ("b", "2"), ("c", "3"), ("d", "4")] {
            add(&store, name, uid);
        }
        store.switch("c").unwrap();
        let expected: Vec<String> = ["c", "a", "b", "d"].iter().map(|s| s.to_string()).collect();
        assert_eq!(store.attempt_order(100), expected);

        // Bench three with different recovery times: a/c recover at 130,
        // d at 400, so b (still healthy) comes first.
        store.record_failure("a", ErrorKind::RateLimit, 429, 100);
        store.record_failure("c", ErrorKind::RateLimit, 429, 100);
        store.record_failure("d", ErrorKind::Auth, 401, 100);
        let order = store.attempt_order(100);
        assert_eq!(order[0], "b");
        assert_eq!(order[1], "a");
        assert_eq!(order[2], "c");
        assert_eq!(order[3], "d");

        // Everything cooling: still a complete, duplicate-free list.
        for name in ["a", "b", "c", "d"] {
            store.record_failure(name, ErrorKind::Auth, 401, 100);
        }
        let order = store.attempt_order(100);
        assert_eq!(order.len(), 4);
        let mut sorted = order.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), 4);
        // Recovery is automatic once the deadline passes.
        assert_eq!(store.attempt_order(100_000)[0], "c");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn attempt_order_has_no_sixty_four_account_limit() {
        let dir = temp_dir("order-70");
        let store = AccountStore::load(&dir, 0);
        for index in 0..70 {
            add(&store, &format!("a{index:02}"), &format!("{index}"));
        }
        let order = store.attempt_order(0);
        assert_eq!(order.len(), 70);
        let mut sorted = order.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), 70);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn failure_accounting_matches_the_shared_counter_rules() {
        let dir = temp_dir("accounting");
        let store = AccountStore::load(&dir, 0);
        add(&store, "a", "1");
        // Request-level and plan-level failures do not touch the counter.
        store.record_failure("a", ErrorKind::Request, 400, 10);
        store.record_failure("a", ErrorKind::Plan, 403, 10);
        let account = store.snapshot("a").unwrap();
        assert_eq!(account.consecutive_failures, 0);
        assert_eq!(account.disabled_until, 0);
        // The first two transient failures do not cool the account.
        store.record_failure("a", ErrorKind::Transient, 500, 10);
        store.record_failure("a", ErrorKind::Transient, 500, 11);
        assert_eq!(store.snapshot("a").unwrap().disabled_until, 0);
        store.record_failure("a", ErrorKind::Transient, 500, 12);
        let account = store.snapshot("a").unwrap();
        assert_eq!(account.consecutive_failures, 3);
        assert_eq!(account.disabled_until, 32);
        assert_eq!(account.last_status, 500);
        // Any success clears everything.
        store.record_success("a", 40);
        let account = store.snapshot("a").unwrap();
        assert_eq!(account.consecutive_failures, 0);
        assert_eq!(account.disabled_until, 0);
        assert_eq!(account.last_status, 200);
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn account_files_are_private() {
        use std::os::unix::fs::PermissionsExt;

        let dir = temp_dir("mode");
        let probe = dir.join("probe");
        fs::write(&probe, b"x").unwrap();
        fs::set_permissions(&probe, fs::Permissions::from_mode(0o600)).unwrap();
        let retained = fs::metadata(&probe).unwrap().permissions().mode() & 0o777 == 0o600;
        if !retained {
            // drvfs and friends drop permission bits; nothing to assert there
            // (spec 50 §11).
            eprintln!("skipping: filesystem does not retain mode bits");
            let _ = fs::remove_dir_all(&dir);
            return;
        }
        let store = AccountStore::load(&dir, 0);
        add(&store, "a", "1");
        store.switch("a").unwrap();
        for name in [ACCOUNTS_FILE, ACTIVE_ACCOUNT_FILE] {
            let mode = fs::metadata(dir.join(name)).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "{name} is {mode:o}");
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn reload_carries_cooldown_state_by_name() {
        let dir = temp_dir("reload");
        let store = AccountStore::load(&dir, 0);
        add(&store, "a", "1");
        store.record_failure("a", ErrorKind::Auth, 401, 10);
        add(&store, "b", "2");
        store.reload().unwrap();
        assert_eq!(store.len(), 2);
        assert_eq!(store.snapshot("a").unwrap().disabled_until, 310);
        assert_eq!(store.current_name(), "a");
        let _ = fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod tests_concurrency {
    use super::*;
    use std::sync::Arc;

    /// Readers and writers race: every failure must be counted (no lost
    /// updates) and a reader must never see a duplicate or missing account
    /// (spec 50 §9 item 1).
    #[test]
    fn concurrent_failures_and_reads_keep_state_consistent() {
        let dir = std::env::temp_dir().join(format!(
            "zprs-accounts-concurrency-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        fs::create_dir_all(&dir).unwrap();
        let store = Arc::new(AccountStore::load(&dir, 0));
        for index in 0..8 {
            store
                .add_account(
                    &format!("a{index}"),
                    &format!("{index}"),
                    serde_json::json!({"token": "x"}),
                )
                .unwrap();
        }

        let mut handles = Vec::new();
        for worker in 0..8u32 {
            let store = Arc::clone(&store);
            handles.push(std::thread::spawn(move || {
                let name = format!("a{worker}");
                for round in 0..200i64 {
                    store.record_failure(&name, ErrorKind::RateLimit, 429, round);
                    let order = store.attempt_order(round);
                    assert_eq!(order.len(), 8);
                    let unique: std::collections::HashSet<_> = order.iter().collect();
                    assert_eq!(unique.len(), 8);
                    let _ = store.switch(&name);
                    let _ = store.snapshot(&name);
                }
            }));
        }
        for handle in handles {
            handle.join().unwrap();
        }
        for index in 0..8 {
            let snapshot = store.snapshot(&format!("a{index}")).unwrap();
            assert_eq!(snapshot.consecutive_failures, 200, "a{index}");
            assert_eq!(snapshot.disabled_until, 199 + COOLDOWN_RATE_LIMIT_S);
        }
        assert!(!store.current_name().is_empty());
        let _ = fs::remove_dir_all(&dir);
    }
}
