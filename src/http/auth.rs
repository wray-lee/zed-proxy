//! Gateway key loading, comparison and credential-header parsing
//! (spec `55-gateway-auth.md` §1–§3).
//!
//! The parsing/loading helpers are pure and take their inputs explicitly so the
//! tests do not have to mutate the process environment.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

pub const DATA_DIR_KEY_FILE: &str = "api_key";
/// File keys are limited to 1 MiB (spec 55 §1.1).
pub const MAX_KEY_FILE_BYTES: u64 = 1024 * 1024;

/// The configured gateway keys. An empty list means authentication is off
/// (only allowed on loopback, spec 55 §1.3 / §4.2).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GatewayKeys {
    keys: Vec<String>,
}

impl GatewayKeys {
    pub fn new(keys: Vec<String>) -> Self {
        Self { keys }
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    pub fn len(&self) -> usize {
        self.keys.len()
    }

    /// Load from the three sources in priority order (spec 55 §1.1).
    ///
    /// * `ZED_API_KEY` — only the empty string counts as "unset"; a non-empty
    ///   value that splits to nothing still wins and yields an empty set.
    /// * `ZED_API_KEY_FILE` — a read failure yields "no keys" and does *not*
    ///   fall through to the data-directory file.
    /// * `<data>/api_key`.
    pub fn load(env_value: Option<&str>, env_file: Option<&str>, data_dir: Option<&Path>) -> Self {
        if let Some(value) = env_value {
            if !value.is_empty() {
                return Self::new(split_keys(value));
            }
        }
        if let Some(path) = env_file {
            if !path.is_empty() {
                return match read_key_file(Path::new(path)) {
                    Some(contents) => Self::new(split_keys(&contents)),
                    None => Self::new(Vec::new()),
                };
            }
        }
        let data_dir_path = data_dir.map_or_else(
            || PathBuf::from(DATA_DIR_KEY_FILE),
            |dir| dir.join(DATA_DIR_KEY_FILE),
        );
        match read_key_file(&data_dir_path) {
            Some(contents) => Self::new(split_keys(&contents)),
            None => Self::new(Vec::new()),
        }
    }

    pub fn from_env() -> Self {
        let env_value = std::env::var("ZED_API_KEY").ok();
        let env_file = std::env::var("ZED_API_KEY_FILE").ok();
        // The process has already chdir'ed into the data directory, so a
        // relative `api_key` resolves there (spec 75 §3).
        Self::load(env_value.as_deref(), env_file.as_deref(), None)
    }

    /// Constant-time comparison over SHA-256 digests, always walking every key
    /// (spec 55 §2.2).
    pub fn verify(&self, presented: &str) -> bool {
        if presented.is_empty() || self.keys.is_empty() {
            return false;
        }
        let presented_digest = Sha256::digest(presented.as_bytes());
        let mut matched = false;
        for key in &self.keys {
            let configured = Sha256::digest(key.as_bytes());
            // Do not short-circuit: every key is compared on every request.
            matched |= bool::from(configured.as_slice().ct_eq(presented_digest.as_slice()));
        }
        matched
    }
}

/// Split on `,`, `\r` and `\n`, trim spaces/tabs, drop empty items
/// (spec 55 §1.2).
pub fn split_keys(raw: &str) -> Vec<String> {
    raw.split([',', '\r', '\n'])
        .map(|item| item.trim_matches([' ', '\t']))
        .filter(|item| !item.is_empty())
        .map(str::to_string)
        .collect()
}

fn read_key_file(path: &Path) -> Option<String> {
    let metadata = std::fs::metadata(path).ok()?;
    if metadata.len() > MAX_KEY_FILE_BYTES {
        return None;
    }
    std::fs::read_to_string(path).ok()
}

/// Does this request need a gateway key? (spec 55 §3.2)
pub fn requires_auth(keys: &GatewayKeys, method: &str, path: &str) -> bool {
    if keys.is_empty() {
        return false;
    }
    if method == "OPTIONS" {
        return false;
    }
    path.starts_with("/v1/") || path.starts_with("/zed/")
}

/// Extract the presented credential from the raw header block
/// (spec 55 §3.3). The last `x-api-key` wins; a well-formed `Authorization:
/// Bearer` wins over it; anything else falls through.
pub fn presented_credential(headers: &[(String, String)]) -> Option<String> {
    let mut api_key: Option<String> = None;
    for (name, value) in headers {
        if name.eq_ignore_ascii_case("authorization") {
            if let Some(token) = bearer_token(value) {
                return Some(token);
            }
        } else if name.eq_ignore_ascii_case("x-api-key") {
            let trimmed = value.trim_matches([' ', '\t']);
            if !trimmed.is_empty() {
                api_key = Some(trimmed.to_string());
            }
        }
    }
    api_key
}

/// `Bearer` prefix, case-insensitive, followed by a space or tab, then a
/// non-empty token (spec 55 §3.3 item 1).
fn bearer_token(value: &str) -> Option<String> {
    let trimmed = value.trim_matches([' ', '\t']);
    let prefix = trimmed.get(..6)?;
    if !prefix.eq_ignore_ascii_case("Bearer") {
        return None;
    }
    let separator = trimmed.as_bytes().get(6).copied()?;
    if separator != b' ' && separator != b'\t' {
        return None;
    }
    let token = trimmed[6..].trim_matches([' ', '\t']);
    if token.is_empty() {
        None
    } else {
        Some(token.to_string())
    }
}

/// Parse the raw header block (`\r\n` separated) into `(name, value)` pairs.
/// Values may contain `:` — only the first colon splits (spec 55 §3.3).
#[cfg(test)]
pub fn parse_header_lines(block: &str) -> Vec<(String, String)> {
    block
        .split("\r\n")
        .filter_map(|line| {
            let (name, value) = line.split_once(':')?;
            Some((
                name.trim_matches([' ', '\t']).to_string(),
                value.trim_matches([' ', '\t']).to_string(),
            ))
        })
        .collect()
}

/// The exact gateway 401 body (spec 55 §3.4, D55-1 elected `auth_error`).
pub const UNAUTHORIZED_BODY: &str =
    r#"{"error":{"message":"missing or invalid API key","type":"auth_error"}}"#;

/// Startup bind decision (spec 55 §4.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindDecision {
    Allow { auth_enabled: bool },
    Refuse,
}

pub fn bind_decision(loopback: bool, key_count: usize) -> BindDecision {
    match (loopback, key_count) {
        (true, count) => BindDecision::Allow {
            auth_enabled: count > 0,
        },
        (false, 0) => BindDecision::Refuse,
        (false, _) => BindDecision::Allow { auth_enabled: true },
    }
}

/// Refusal line printed to stderr before exiting with code 1 (spec 55 §4.2).
pub const REFUSAL_MESSAGE: &str = "refusing to bind: 监听地址非回环 (non-loopback) 且未配置 API key；必须配置 ZED_API_KEY 或 <data>/api_key，或改回 127.0.0.1";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_on_commas_and_newlines() {
        assert_eq!(
            split_keys(" key1 , key2\n\n key3 \r\n"),
            vec!["key1", "key2", "key3"]
        );
        assert_eq!(split_keys("a,,b,"), vec!["a", "b"]);
        assert_eq!(split_keys("  \t \n "), Vec::<String>::new());
    }

    #[test]
    fn environment_value_wins_over_the_file() {
        let dir = tempdir();
        std::fs::write(dir.join(DATA_DIR_KEY_FILE), "file-key").unwrap();
        let keys = GatewayKeys::load(Some("  env-a ,env-b  \n env-c "), None, Some(&dir));
        assert_eq!(keys.len(), 3);
        assert!(keys.verify("env-c"));
        assert!(!keys.verify("file-key"));
    }

    #[test]
    fn a_whitespace_only_environment_value_disables_fallthrough() {
        // Non-empty ZED_API_KEY that splits to nothing means "no keys", and must
        // not fall back to the file source (spec 55 §1.1).
        let dir = tempdir();
        std::fs::write(dir.join(DATA_DIR_KEY_FILE), "file-key").unwrap();
        let keys = GatewayKeys::load(Some("  "), None, Some(&dir));
        assert!(keys.is_empty());
    }

    #[test]
    fn key_file_source_and_its_failure_mode() {
        let dir = tempdir();
        let file = dir.join("keys.txt");
        std::fs::write(&file, "one,two").unwrap();
        let keys = GatewayKeys::load(None, Some(file.to_str().unwrap()), Some(&dir));
        assert!(keys.verify("one") && keys.verify("two"));

        // A missing explicit file yields no keys and does not fall back.
        std::fs::write(dir.join(DATA_DIR_KEY_FILE), "data-dir-key").unwrap();
        let keys = GatewayKeys::load(
            None,
            Some(dir.join("missing.txt").to_str().unwrap()),
            Some(&dir),
        );
        assert!(keys.is_empty());
    }

    #[test]
    fn data_directory_file_is_the_last_source() {
        let dir = tempdir();
        std::fs::write(dir.join(DATA_DIR_KEY_FILE), "comma,separated").unwrap();
        let keys = GatewayKeys::load(None, None, Some(&dir));
        assert_eq!(keys.len(), 2);
        assert!(keys.verify("separated"));
    }

    #[test]
    fn verify_rejects_prefixes_wrong_case_and_empty() {
        let keys = GatewayKeys::new(vec!["key-a".into(), "key-b".into()]);
        assert!(keys.verify("key-a"));
        assert!(keys.verify("key-b"));
        for presented in ["key-c", "key-a-longer", "key-", "", "Key-a"] {
            assert!(!keys.verify(presented), "{presented:?} must be rejected");
        }
        assert!(!GatewayKeys::new(Vec::new()).verify("anything"));
    }

    #[test]
    fn bearer_parsing_tolerates_case_and_whitespace() {
        let headers = parse_header_lines("authorization:   bEaReR    secret-token  \r\n");
        assert_eq!(
            presented_credential(&headers).as_deref(),
            Some("secret-token")
        );
    }

    #[test]
    fn x_api_key_fallback_rules() {
        let only_header = parse_header_lines("x-api-key: k\r\n");
        assert_eq!(presented_credential(&only_header).as_deref(), Some("k"));

        let basic_plus_key = parse_header_lines("Authorization: Basic abc\r\nx-api-key: k\r\n");
        assert_eq!(presented_credential(&basic_plus_key).as_deref(), Some("k"));

        let bare_bearer = parse_header_lines("Authorization: Bearer\r\nx-api-key: k\r\n");
        assert_eq!(presented_credential(&bare_bearer).as_deref(), Some("k"));

        let none = parse_header_lines("Authorization: Basic abc\r\n");
        assert_eq!(presented_credential(&none), None);
    }

    #[test]
    fn last_x_api_key_wins_and_first_bearer_wins() {
        let headers = parse_header_lines("x-api-key: first\r\nx-api-key: second\r\n");
        assert_eq!(presented_credential(&headers).as_deref(), Some("second"));

        let headers =
            parse_header_lines("Authorization: Bearer one\r\nAuthorization: Bearer two\r\n");
        assert_eq!(presented_credential(&headers).as_deref(), Some("one"));
    }

    #[test]
    fn protected_prefix_matrix() {
        let keys = GatewayKeys::new(vec!["k".into()]);
        for path in [
            "/v1/models",
            "/v1/",
            "/zed/",
            "/zed/accounts",
            "/zed/login/status",
        ] {
            assert!(
                requires_auth(&keys, "GET", path),
                "{path} must be protected"
            );
        }
        for path in [
            "/",
            "/health",
            "/api/event_logging/batch",
            "/zed",
            "/v1",
            "/V1/models",
            "",
        ] {
            assert!(!requires_auth(&keys, "GET", path), "{path} must be public");
        }
        assert!(!requires_auth(&keys, "OPTIONS", "/zed/accounts"));
        assert!(!requires_auth(&GatewayKeys::default(), "GET", "/v1/models"));
    }

    #[test]
    fn bind_policy_matrix() {
        assert_eq!(
            bind_decision(true, 0),
            BindDecision::Allow {
                auth_enabled: false
            }
        );
        assert_eq!(
            bind_decision(true, 2),
            BindDecision::Allow { auth_enabled: true }
        );
        assert_eq!(
            bind_decision(false, 1),
            BindDecision::Allow { auth_enabled: true }
        );
        assert_eq!(bind_decision(false, 0), BindDecision::Refuse);
        assert!(REFUSAL_MESSAGE.contains("监听地址非回环"));
        assert!(REFUSAL_MESSAGE.contains("未配置 API key"));
    }

    /// Test data directory removed on drop, so a failed test leaves nothing
    /// behind either.
    struct TempDir(PathBuf);

    impl std::ops::Deref for TempDir {
        type Target = std::path::Path;

        fn deref(&self) -> &Self::Target {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn tempdir() -> TempDir {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("zed-proxy-auth-test-{}-{id}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }
}
