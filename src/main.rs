//! `zed-proxy` entry point and CLI (spec `75-config-and-cli.md` §2).
//!
//! Subcommands: `serve [port]` (default), `login [name]`, `accounts`,
//! `healthcheck [--url <url>]`.

#![forbid(unsafe_code)]
// This crate is the scaffold for the Rust rewrite: every module interface and
// neutral type is declared up front, so most items have no caller until the
// follow-up implementation tasks land. Drop these two allowances once the
// modules are implemented (they are the only reason the lints are quiet).
#![allow(dead_code, unused_imports)]

mod accounts;
mod catalog;
mod client;
mod config;
mod error;
mod http;
mod login;
mod neutral;
mod orb;
mod probe;
mod provider;
mod proxy;
mod upstream;

use std::process::ExitCode;
use std::time::{Duration, Instant};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let command = args.first().map(String::as_str);
    let rest = &args[args.len().min(1)..];
    match command {
        None | Some("serve") => {
            exit_code(http::server::run_serve(rest.first().map(String::as_str)))
        }
        Some("login") => match login::run_cli_login(rest.first().map(String::as_str)) {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("[login] {}", err.message);
                ExitCode::FAILURE
            }
        },
        Some("accounts") => run_accounts(),
        Some("healthcheck") => exit_code(run_healthcheck(rest)),
        Some("--help" | "-h" | "help") => {
            print_usage();
            ExitCode::SUCCESS
        }
        Some(other) => {
            eprintln!("[zed-proxy] unknown command: {other}");
            print_usage();
            ExitCode::FAILURE
        }
    }
}

fn exit_code(code: i32) -> ExitCode {
    if code == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn print_usage() {
    eprintln!(
        "usage: zed-proxy [serve [port] | login [name] | accounts | healthcheck [--url <url>]]"
    );
}

/// `accounts` — print the configured accounts, no network access (spec 75 §2.3).
fn run_accounts() -> ExitCode {
    let data_dir = std::env::var("ZED_DATA_DIR")
        .ok()
        .filter(|value| !value.trim().is_empty());
    if let Some(dir) = data_dir.as_deref() {
        if let Err(err) = std::env::set_current_dir(dir) {
            eprintln!("[zed-proxy] cannot use ZED_DATA_DIR {dir:?}: {err}");
            return ExitCode::FAILURE;
        }
    }
    let store = accounts::AccountStore::load(".", http::server::now_unix());
    let entries = store.snapshots();
    if entries.is_empty() {
        eprintln!("No accounts. Run: zed-proxy login");
        return ExitCode::SUCCESS;
    }
    for account in entries {
        println!("  {} (uid: {})", account.name, account.user_id);
    }
    ExitCode::SUCCESS
}

/// `healthcheck [--url <url>]` (spec 75 §2.4): GET the health endpoint with a
/// 3 second budget; exit 0 only for HTTP 200 with `{"status":"ok"}`.
fn run_healthcheck(args: &[String]) -> i32 {
    let url = match parse_healthcheck_args(args) {
        Ok(url) => url,
        Err(message) => {
            eprintln!("[healthcheck] {message}");
            return 1;
        }
    };
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            eprintln!("[healthcheck] cannot start runtime: {err}");
            return 1;
        }
    };
    runtime.block_on(async move {
        let client = match reqwest::Client::builder()
            .timeout(Duration::from_secs(3))
            .build()
        {
            Ok(client) => client,
            Err(err) => {
                eprintln!("[healthcheck] cannot build HTTP client: {err}");
                return 1;
            }
        };
        let started = Instant::now();
        let response = match client.get(&url).send().await {
            Ok(response) => response,
            Err(err) => {
                eprintln!("[healthcheck] request failed: {err}");
                return 1;
            }
        };
        let status = response.status().as_u16();
        if status != 200 {
            eprintln!(
                "[healthcheck] unexpected status {status} after {:?}",
                started.elapsed()
            );
            return 1;
        }
        let body = match response.text().await {
            Ok(body) => body,
            Err(err) => {
                eprintln!("[healthcheck] cannot read body: {err}");
                return 1;
            }
        };
        let healthy = serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .and_then(|value| {
                value
                    .get("status")
                    .and_then(serde_json::Value::as_str)
                    .map(|status| status == "ok")
            })
            .unwrap_or(false);
        if healthy {
            0
        } else {
            eprintln!("[healthcheck] unexpected body: {body}");
            1
        }
    })
}

/// Default URL is `http://127.0.0.1:<ZED_API_PORT or 8001>/health`
/// (spec 75 §2.4).
fn parse_healthcheck_args(args: &[String]) -> Result<String, String> {
    let mut url = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--url" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| "--url requires a value".to_string())?;
                url = Some(value.clone());
                index += 2;
            }
            other if other.starts_with("--url=") => {
                url = Some(other.trim_start_matches("--url=").to_string());
                index += 1;
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    let url = url.unwrap_or_else(|| format!("http://127.0.0.1:{}/health", config::port_from_env()));
    let parsed = url::Url::parse(&url).map_err(|_| format!("invalid healthcheck url: {url}"))?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        return Err(format!("invalid healthcheck url: {url}"));
    }
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn healthcheck_defaults_to_the_loopback_health_endpoint() {
        let url = parse_healthcheck_args(&[]).unwrap();
        assert!(url.starts_with("http://127.0.0.1:"));
        assert!(url.ends_with("/health"));
    }

    #[test]
    fn healthcheck_url_flag_accepts_both_spellings() {
        let args = vec!["--url".to_string(), "http://x/health".to_string()];
        assert_eq!(parse_healthcheck_args(&args).unwrap(), "http://x/health");
        let args = vec!["--url=http://y/health".to_string()];
        assert_eq!(parse_healthcheck_args(&args).unwrap(), "http://y/health");
    }

    #[test]
    fn healthcheck_rejects_bad_arguments() {
        assert!(parse_healthcheck_args(&["--url".to_string()]).is_err());
        assert!(parse_healthcheck_args(&["ftp://x".to_string()]).is_err());
        assert!(parse_healthcheck_args(&["--url".to_string(), "ftp://x".to_string()]).is_err());
    }
}
