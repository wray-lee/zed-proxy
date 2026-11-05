//! Process configuration from the environment (spec `75-config-and-cli.md` §1).
//!
//! Values are read once at startup; runtime changes to the environment have no
//! effect (spec 75 §1). Parsing helpers take the raw value as an argument so the
//! behaviour is testable without mutating the process environment.

use std::fmt;
use std::net::IpAddr;
use std::path::PathBuf;

pub const DEFAULT_HOST: &str = "127.0.0.1";
pub const DEFAULT_PORT: u16 = 8001;
pub const DEFAULT_LOGIN_PORT: u16 = 58443;
pub const DEFAULT_UPSTREAM_BASE: &str = "https://cloud.zed.dev";
/// Outbound proxy default port when the URI has none (spec 75 §1.1; not 8080).
pub const DEFAULT_PROXY_PORT: u16 = 7890;

/// A validated `http(s)://host[:port][/prefix]` base URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpstreamBase {
    /// Scheme + authority + optional prefix, without a trailing `/`.
    pub prefix: String,
}

impl UpstreamBase {
    pub fn parse(raw: &str) -> Result<Self, ConfigError> {
        let trimmed = raw.trim();
        let without_slash = trimmed.trim_end_matches('/');
        let parsed = url::Url::parse(without_slash)
            .map_err(|_| ConfigError::InvalidUpstreamBase(trimmed.to_string()))?;
        if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
            return Err(ConfigError::InvalidUpstreamBase(trimmed.to_string()));
        }
        Ok(Self {
            prefix: without_slash.to_string(),
        })
    }

    pub fn join(&self, path: &str) -> String {
        format!("{}/{}", self.prefix, path.trim_start_matches('/'))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    InvalidUpstreamBase(String),
    InvalidBindHost(String),
    InvalidDataDir(String),
    Io(String),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::InvalidUpstreamBase(value) => write!(
                f,
                "ZED_UPSTREAM_BASE must be http(s)://host[:port][/prefix], got {value:?}"
            ),
            ConfigError::InvalidBindHost(value) => write!(
                f,
                "ZED_API_HOST must be an IP literal or localhost, got {value:?}"
            ),
            ConfigError::InvalidDataDir(value) => {
                write!(f, "cannot use ZED_DATA_DIR {value:?}")
            }
            ConfigError::Io(value) => write!(f, "{value}"),
        }
    }
}

impl std::error::Error for ConfigError {}

/// Outbound proxy, resolved once (spec 75 §1.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyConfig {
    pub scheme: String,
    pub host: String,
    pub port: u16,
}

impl ProxyConfig {
    /// First usable value of `HTTPS_PROXY`, `https_proxy`, `ALL_PROXY`, `all_proxy`.
    pub fn from_env() -> Option<Self> {
        ["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy"]
            .iter()
            .filter_map(|name| std::env::var(name).ok())
            .find_map(|raw| Self::parse(&raw))
    }

    pub fn parse(raw: &str) -> Option<Self> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return None;
        }
        let with_scheme = if trimmed.contains("://") {
            trimmed.to_string()
        } else {
            format!("http://{trimmed}")
        };
        let parsed = url::Url::parse(&with_scheme).ok()?;
        let scheme = parsed.scheme().to_string();
        if !matches!(scheme.as_str(), "http" | "https") {
            return None;
        }
        let host = parsed.host_str()?;
        if host.is_empty() {
            return None;
        }
        Some(Self {
            scheme,
            host: host.to_string(),
            port: parsed.port().unwrap_or(DEFAULT_PROXY_PORT),
        })
    }
}

/// Everything `serve` needs, already validated.
#[derive(Debug, Clone)]
pub struct Config {
    pub host: String,
    pub port: u16,
    pub login_port: u16,
    pub data_dir: Option<PathBuf>,
    pub system_id: Option<String>,
    pub upstream: UpstreamBase,
    pub proxy: Option<ProxyConfig>,
}

impl Config {
    pub fn from_env() -> Result<Self, ConfigError> {
        let host = host_from_env();
        if !is_bindable_host(&host) {
            return Err(ConfigError::InvalidBindHost(host));
        }
        Ok(Self {
            host,
            port: port_from_env(),
            // An invalid ZED_LOGIN_PORT is not a startup failure: only the
            // login endpoint reports it (400, spec 60 §3), so the default is
            // kept here.
            login_port: login_port_from_env().unwrap_or(DEFAULT_LOGIN_PORT),
            data_dir: data_dir_from_env(),
            system_id: non_empty_env("ZED_SYSTEM_ID"),
            upstream: upstream_from_env()?,
            proxy: ProxyConfig::from_env(),
        })
    }

    pub fn is_loopback(&self) -> bool {
        is_loopback_host(&self.host)
    }

    /// The address the listener binds. `localhost` maps to `127.0.0.1`
    /// (spec 55 §4.1).
    pub fn bind_host(&self) -> &str {
        if self.host.eq_ignore_ascii_case("localhost") {
            "127.0.0.1"
        } else {
            &self.host
        }
    }
}

pub fn host_from_env() -> String {
    match std::env::var("ZED_API_HOST") {
        Ok(value) if !value.trim().is_empty() => value.trim().to_string(),
        _ => DEFAULT_HOST.to_string(),
    }
}

/// `ZED_API_PORT` parsing: missing, empty or invalid falls back to 8001
/// (spec 75 §1).
pub fn parse_port_arg(value: &str) -> Option<u16> {
    let trimmed = value.trim();
    if trimmed.is_empty() || !trimmed.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    trimmed.parse::<u16>().ok().filter(|port| *port != 0)
}

pub fn port_from_env() -> u16 {
    std::env::var("ZED_API_PORT")
        .ok()
        .and_then(|value| parse_port_arg(&value))
        .unwrap_or(DEFAULT_PORT)
}

/// Port resolution for `serve [port]`: the positional argument wins; an invalid
/// argument falls back to 8001 *without* consulting the environment
/// (spec 75 §2.1). The environment is read only when no argument was given.
pub fn resolve_serve_port(arg: Option<&str>, env_value: Option<&str>) -> u16 {
    match arg {
        Some(value) => parse_port_arg(value).unwrap_or(DEFAULT_PORT),
        None => env_value.and_then(parse_port_arg).unwrap_or(DEFAULT_PORT),
    }
}

pub fn login_port_from_env() -> Result<u16, ()> {
    login_port_setting(std::env::var("ZED_LOGIN_PORT").ok().as_deref())
}

/// `Ok(default)` when unset/empty; `Err(())` when the value is not a valid port
/// (the login endpoint turns that into a 400, spec 60 §3).
pub fn login_port_setting(raw: Option<&str>) -> Result<u16, ()> {
    match raw {
        None => Ok(DEFAULT_LOGIN_PORT),
        Some(value) if value.trim().is_empty() => Ok(DEFAULT_LOGIN_PORT),
        Some(value) => parse_port_arg(value).ok_or(()),
    }
}

fn data_dir_from_env() -> Option<PathBuf> {
    match std::env::var("ZED_DATA_DIR") {
        Ok(value) if !value.trim().is_empty() => Some(PathBuf::from(value)),
        _ => None,
    }
}

fn non_empty_env(name: &str) -> Option<String> {
    match std::env::var(name) {
        Ok(value) if !value.is_empty() => Some(value),
        _ => None,
    }
}

fn upstream_from_env() -> Result<UpstreamBase, ConfigError> {
    match std::env::var("ZED_UPSTREAM_BASE") {
        Ok(value) if !value.trim().is_empty() => UpstreamBase::parse(&value),
        _ => Ok(UpstreamBase {
            prefix: DEFAULT_UPSTREAM_BASE.to_string(),
        }),
    }
}

/// Loopback matrix (spec 55 §4.1). Only `localhost`, IPv4 literals and three
/// IPv6 spellings count; everything else (including `::ffff:127.0.0.1` and
/// invalid literals such as `127.0.0.256`) is non-loopback.
pub fn is_loopback_host(host: &str) -> bool {
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    if matches!(host, "::1" | "[::1]" | "0:0:0:0:0:0:0:1") {
        return true;
    }
    match host.parse::<IpAddr>() {
        Ok(IpAddr::V4(v4)) => v4.octets()[0] == 127,
        _ => false,
    }
}

/// Whether the configured host is a valid binding target: an IP literal or
/// `localhost` (spec 55 §4.1). Other hostnames are refused at startup.
pub fn is_bindable_host(host: &str) -> bool {
    if host.is_empty() {
        return false;
    }
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    if matches!(host, "::1" | "[::1]" | "0:0:0:0:0:0:0:1") {
        return true;
    }
    host.parse::<IpAddr>().is_ok()
}

/// Create `ZED_DATA_DIR` if needed and `chdir` into it, before any state is
/// loaded (spec 75 §1).
pub fn prepare_data_dir(dir: Option<&PathBuf>) -> Result<(), ConfigError> {
    let Some(dir) = dir else {
        return Ok(());
    };
    if let Err(err) = std::fs::create_dir_all(dir) {
        return Err(ConfigError::InvalidDataDir(format!(
            "{}: {err}",
            dir.display()
        )));
    }
    if let Err(err) = std::env::set_current_dir(dir) {
        return Err(ConfigError::InvalidDataDir(format!(
            "{}: {err}",
            dir.display()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn port_parsing_rejects_junk_and_zero() {
        assert_eq!(parse_port_arg("8001"), Some(8001));
        assert_eq!(parse_port_arg(" 65535 "), Some(65535));
        assert_eq!(parse_port_arg("0"), None);
        assert_eq!(parse_port_arg("65536"), None);
        assert_eq!(parse_port_arg("-1"), None);
        assert_eq!(parse_port_arg("80 01"), None);
        assert_eq!(parse_port_arg(""), None);
    }

    #[test]
    fn positional_port_wins_and_invalid_falls_back_to_default() {
        assert_eq!(resolve_serve_port(Some("7000"), Some("9000")), 7000);
        // An invalid positional argument falls back to 8001, ignoring the env.
        assert_eq!(resolve_serve_port(Some("nope"), Some("9000")), DEFAULT_PORT);
        assert_eq!(resolve_serve_port(None, Some("9000")), 9000);
        assert_eq!(resolve_serve_port(None, Some("nope")), DEFAULT_PORT);
        assert_eq!(resolve_serve_port(None, None), DEFAULT_PORT);
    }

    #[test]
    fn login_port_setting_distinguishes_absent_from_invalid() {
        assert_eq!(login_port_setting(None), Ok(DEFAULT_LOGIN_PORT));
        assert_eq!(login_port_setting(Some("  ")), Ok(DEFAULT_LOGIN_PORT));
        assert_eq!(login_port_setting(Some("58500")), Ok(58500));
        assert_eq!(login_port_setting(Some("0")), Err(()));
        assert_eq!(login_port_setting(Some("x")), Err(()));
    }

    #[test]
    fn loopback_matrix_matches_spec() {
        for host in [
            "localhost",
            "LocalHost",
            "127.0.0.1",
            "127.0.0.53",
            "127.255.255.255",
            "::1",
            "[::1]",
            "0:0:0:0:0:0:0:1",
        ] {
            assert!(is_loopback_host(host), "{host} should be loopback");
        }
        for host in [
            "0.0.0.0",
            "::",
            "[::]",
            "192.0.2.10",
            "198.51.100.5",
            "203.0.113.1",
            "example.com",
            "",
            "127.0.0.256",
            "0:0:0:0:0:0:0:2",
            "::ffff:127.0.0.1",
        ] {
            assert!(!is_loopback_host(host), "{host} should not be loopback");
        }
    }

    #[test]
    fn bindable_hosts_are_ip_literals_or_localhost() {
        assert!(is_bindable_host("127.0.0.1"));
        assert!(is_bindable_host("0.0.0.0"));
        assert!(is_bindable_host("localhost"));
        assert!(is_bindable_host("::1"));
        assert!(!is_bindable_host("example.com"));
        assert!(!is_bindable_host(""));
    }

    #[test]
    fn upstream_base_strips_trailing_slash_and_validates_scheme() {
        let base = UpstreamBase::parse("http://127.0.0.1:9000/fake/").unwrap();
        assert_eq!(
            base.join("completions"),
            "http://127.0.0.1:9000/fake/completions"
        );
        assert!(UpstreamBase::parse("ftp://example.com").is_err());
        assert!(UpstreamBase::parse("cloud.zed.dev").is_err());
        assert_eq!(
            UpstreamBase::parse(DEFAULT_UPSTREAM_BASE).unwrap().prefix,
            "https://cloud.zed.dev"
        );
    }

    #[test]
    fn proxy_default_port_is_7890() {
        let proxy = ProxyConfig::parse("http://127.0.0.1").unwrap();
        assert_eq!(proxy.port, DEFAULT_PROXY_PORT);
        let proxy = ProxyConfig::parse("proxy.example:3128").unwrap();
        assert_eq!(proxy.port, 3128);
        assert_eq!(proxy.host, "proxy.example");
        assert!(ProxyConfig::parse("   ").is_none());
    }
}
