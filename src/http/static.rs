//! The management page served at `GET /` (spec 70 §3.1).
//!
//! `webui/dist/index.html` is embedded at compile time; `build.rs` fails the
//! build with a clear message when it is missing, so a broken page can never be
//! shipped silently.

/// Full single-file management UI (JS/CSS inlined by the WebUI build).
pub const INDEX_HTML: &[u8] = include_bytes!("../../webui/dist/index.html");

pub const CONTENT_TYPE: &str = "text/html; charset=utf-8";
/// Only this route carries `Cache-Control` (spec 70 §3.1, spec 80 §5.5 item 2).
pub const CACHE_CONTROL: &str = "no-store";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_page_is_a_complete_html_document() {
        let html = std::str::from_utf8(INDEX_HTML).expect("index.html is UTF-8");
        assert!(html.contains("<!doctype html") || html.contains("<!DOCTYPE html"));
        assert!(html.contains("<html"));
    }

    #[test]
    fn embedded_page_contains_every_navigation_page() {
        let html = std::str::from_utf8(INDEX_HTML).unwrap();
        for page in ["accounts", "models", "health", "endpoints", "integration"] {
            assert!(
                html.contains(&format!("page-{page}")),
                "webui/dist/index.html is missing the {page} page"
            );
        }
    }
}
