//! Hardened HTTP client for requests that leave the machine (Chud's Workers,
//! GitHub). Unlike `lcu::build_lcu_client`'s loopback-only, cert-relaxed client:
//! default TLS validation, a host allowlist enforced on the initial request AND
//! every redirect hop, and size-capped reads against a hostile upstream.
//!
//! `chud-skins`/`chud-runes`/GitHub responses are untrusted input — every
//! external call site must funnel through [`check_external_url`] (via
//! [`get_json_checked`]/[`get_bytes_checked`]) rather than re-deriving "safe."

use std::collections::HashSet;
use std::time::Duration;

/// Built-in external hosts Chud talks to regardless of config (its Workers +
/// GitHub infra). Config-derived hosts are folded in on top — see [`allowed_origins`].
const BUILT_IN_HOSTS: &[&str] = &[
    "chud-runes.jivy26.workers.dev",
    "chud-skins.jivy26.workers.dev",
    "chud-party-relay.jivy26.workers.dev",
    "github.com",
    "api.github.com",
    "raw.githubusercontent.com",
    "objects.githubusercontent.com",
    "codeload.github.com",
    "release-assets.githubusercontent.com",
    // Library skin downloads resolve to direct RuneForge R2 artifact URLs —
    // RuneForge hosts the files, we only host the index + thumbnails.
    "runeforge.dev",
    "r2-prod.runeforge.dev",
];

/// Allowlist for this run: built-ins plus hosts from configured Worker/relay
/// endpoints, `CHUD_RELAY_URL`, and operator extras. Lowercased throughout for
/// case-insensitive comparison against [`check_external_url`]'s request host.
pub fn allowed_origins(cfg: &crate::config::Config) -> HashSet<String> {
    let mut set: HashSet<String> = BUILT_IN_HOSTS.iter().map(|h| h.to_string()).collect();

    for url in [&cfg.runes.endpoint, &cfg.library.endpoint, &cfg.skins.party_relay_url] {
        if let Some(host) = host_of(url) {
            set.insert(host);
        }
    }
    if let Ok(relay) = std::env::var("CHUD_RELAY_URL") {
        if let Some(host) = host_of(&relay) {
            set.insert(host);
        }
    }
    for extra in &cfg.network.extra_allowed_origins {
        let host = extra.trim().to_lowercase();
        if !host.is_empty() {
            set.insert(host);
        }
    }
    set
}

fn host_of(url: &str) -> Option<String> {
    reqwest::Url::parse(url).ok()?.host_str().map(|h| h.to_lowercase())
}

/// Built-in hosts only, no config lookup — for callers with no `Config` handle
/// at their call site (`skins::downloads`'s GitHub client).
pub fn built_in_allowed_origins() -> HashSet<String> {
    BUILT_IN_HOSTS.iter().map(|h| h.to_string()).collect()
}

/// Loopback/link-local/private ranges are never a legitimate external host, even
/// if a config typo puts one in the allowlist — belt-and-suspenders check.
fn is_loopback_or_private(ip: &std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => v4.is_loopback() || v4.is_private() || v4.is_link_local() || v4.is_unspecified(),
        std::net::IpAddr::V6(v6) => v6.is_loopback() || v6.is_unspecified(),
    }
}

/// Shared check for the initial request and every redirect hop: lowercase,
/// reject `localhost`/loopback/private IP literals, then require allowlist membership.
fn host_is_allowed(host: &str, allowed: &HashSet<String>) -> bool {
    let host = host.to_lowercase();
    if host == "localhost" {
        return false;
    }
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        if is_loopback_or_private(&ip) {
            return false;
        }
    }
    allowed.contains(&host)
}

/// Gate every outbound external request: must parse, must be `https`, host must
/// be in `allowed`. Returns the parsed `Url` so callers don't re-parse.
pub fn check_external_url(url: &str, allowed: &HashSet<String>) -> Result<reqwest::Url, String> {
    let parsed = reqwest::Url::parse(url).map_err(|e| format!("invalid URL '{url}': {e}"))?;
    if parsed.scheme() != "https" {
        return Err(format!("refusing non-https URL: {url}"));
    }
    let host = parsed.host_str().ok_or_else(|| format!("URL has no host: {url}"))?;
    if !host_is_allowed(host, allowed) {
        return Err(format!("host not allowed: {host}"));
    }
    Ok(parsed)
}

/// Per-hop redirect check, factored out of [`build_external_client`]'s closure so
/// it's unit-testable without an HTTP server: target must be `https` and allowed.
fn redirect_hop_allowed(url: &reqwest::Url, allowed: &HashSet<String>) -> bool {
    url.scheme() == "https" && url.host_str().map(|h| host_is_allowed(h, allowed)).unwrap_or(false)
}

/// `Chud/{version}` User-Agent — same style as `skins::downloads`'s client.
fn user_agent() -> String {
    format!("Chud/{}", env!("CARGO_PKG_VERSION"))
}

/// Build a client for requests leaving the machine: default cert validation,
/// HTTPS-only, redirect policy re-validates every hop against `allowed` (max 5
/// hops, any non-https/unapproved hop aborts). `allowed` is consumed into the
/// redirect closure — clone before moving in if needed again for
/// [`get_json_checked`]/[`get_bytes_checked`].
pub fn build_external_client(timeout_secs: f64, allowed: HashSet<String>) -> reqwest::Client {
    reqwest::Client::builder()
        .https_only(true)
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs_f64(timeout_secs.max(0.5)))
        .user_agent(user_agent())
        .redirect(reqwest::redirect::Policy::custom(move |attempt| {
            if attempt.previous().len() >= 5 {
                return attempt.error("too many redirects");
            }
            if redirect_hop_allowed(attempt.url(), &allowed) {
                attempt.follow()
            } else {
                attempt.error("redirect to unapproved host")
            }
        }))
        .build()
        .expect("failed to build reqwest client")
}

/// Checked GET returning parsed JSON: validates the URL, requires 2xx, enforces
/// `max_bytes` (via `Content-Length` when present, and while streaming regardless).
pub async fn get_json_checked(
    client: &reqwest::Client,
    url: &str,
    allowed: &HashSet<String>,
    max_bytes: u64,
) -> Result<serde_json::Value, String> {
    let bytes = get_bytes_checked(client, url, allowed, max_bytes).await?;
    serde_json::from_slice(&bytes).map_err(|e| format!("invalid JSON from {url}: {e}"))
}

/// Checked GET returning raw bytes, size-capped. See [`get_json_checked`]
/// for the shared validation/status/size-cap behavior.
pub async fn get_bytes_checked(
    client: &reqwest::Client,
    url: &str,
    allowed: &HashSet<String>,
    max_bytes: u64,
) -> Result<Vec<u8>, String> {
    let checked_url = check_external_url(url, allowed)?;
    let resp = client.get(checked_url).send().await.map_err(|e| e.to_string())?;
    let resp = resp.error_for_status().map_err(|e| e.to_string())?;
    if let Some(len) = resp.content_length() {
        if len > max_bytes {
            return Err(format!("response too large ({len} bytes > {max_bytes}-byte cap) for {url}"));
        }
    }
    let mut resp = resp;
    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(|e| e.to_string())? {
        buf.extend_from_slice(&chunk);
        if buf.len() as u64 > max_bytes {
            return Err(format!("response exceeded {max_bytes}-byte cap for {url}"));
        }
    }
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn allowed() -> HashSet<String> {
        ["chud-skins.jivy26.workers.dev".to_string()].into_iter().collect()
    }

    #[test]
    fn http_scheme_rejected() {
        let err = check_external_url("http://chud-skins.jivy26.workers.dev/all", &allowed()).unwrap_err();
        assert!(err.contains("https"), "unexpected error: {err}");
    }

    #[test]
    fn unapproved_host_rejected() {
        let err = check_external_url("https://evil.example.com/all", &allowed()).unwrap_err();
        assert!(err.contains("not allowed"), "unexpected error: {err}");
    }

    #[test]
    fn approved_host_ok() {
        assert!(check_external_url("https://chud-skins.jivy26.workers.dev/all", &allowed()).is_ok());
    }

    #[test]
    fn loopback_rejected_even_if_present_in_the_allowlist() {
        let mut allow = allowed();
        allow.insert("127.0.0.1".to_string());
        let err = check_external_url("https://127.0.0.1:12345/all", &allow).unwrap_err();
        assert!(err.contains("not allowed"), "unexpected error: {err}");
    }

    #[test]
    fn redirect_hop_allowed_matches_check_external_url() {
        let evil = reqwest::Url::parse("https://evil.example.com/x").unwrap();
        assert!(!redirect_hop_allowed(&evil, &allowed()));
        let ok = reqwest::Url::parse("https://chud-skins.jivy26.workers.dev/x").unwrap();
        assert!(redirect_hop_allowed(&ok, &allowed()));
        let downgraded = reqwest::Url::parse("http://chud-skins.jivy26.workers.dev/x").unwrap();
        assert!(!redirect_hop_allowed(&downgraded, &allowed()));
    }

    /// Online: proves the hardened client rejects a self-signed cert, unlike
    /// `lcu::build_lcu_client`. `#[ignore]`d; run with `cargo test -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn invalid_external_cert_rejected() {
        let mut allow = HashSet::new();
        allow.insert("self-signed.badssl.com".to_string());
        let client = build_external_client(10.0, allow.clone());
        let result = get_bytes_checked(&client, "https://self-signed.badssl.com/", &allow, 1024 * 1024).await;
        assert!(result.is_err(), "expected a self-signed cert to be rejected");
    }

    /// Online: same idea for an expired cert. `#[ignore]`d — needs network.
    #[tokio::test]
    #[ignore]
    async fn expired_cert_rejected() {
        let mut allow = HashSet::new();
        allow.insert("expired.badssl.com".to_string());
        let client = build_external_client(10.0, allow.clone());
        let result = get_bytes_checked(&client, "https://expired.badssl.com/", &allow, 1024 * 1024).await;
        assert!(result.is_err(), "expected an expired cert to be rejected");
    }
}
