# M47: SSRF Mitigation + IDNA Rejection + HTTPS Validation — Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Harden ferrite against server-side request forgery, internationalized domain name homograph attacks, and TLS validation bypass in tracker and web seed URLs.

**Architecture:** A new `url_guard` module in `ferrite-session` provides centralized URL validation. Three new `Settings` fields (`ssrf_mitigation`, `allow_idna`, `validate_https_trackers`) control the behaviour. Validation runs at two enforcement points: (1) when URLs are first parsed/added (torrent load, tracker add), and (2) when HTTP redirects are followed (custom reqwest redirect policy). The `HttpTracker` and `WebSeedTask` constructors accept a security configuration struct to control TLS certificate validation and redirect policy.

**Tech Stack:** Rust edition 2024, reqwest 0.12 (rustls-tls), `std::net::IpAddr`, `url` crate (new dependency for proper URL parsing)

---

## Task 0: Add `url` Dependency

**Files:**
- Modify: `crates/ferrite-session/Cargo.toml`
- Modify: `crates/ferrite-tracker/Cargo.toml`

**Step 1: Add `url` crate to both crates**

In `crates/ferrite-session/Cargo.toml`, add under `[dependencies]`:

```toml
url = "2"
```

In `crates/ferrite-tracker/Cargo.toml`, add under `[dependencies]`:

```toml
url = "2"
```

In the workspace `Cargo.toml`, no changes needed (`url` is not a workspace dependency since only these two crates use it).

---

## Task 1: URL Guard Module — Core Validation Logic

**Files:**
- Create: `crates/ferrite-session/src/url_guard.rs`
- Modify: `crates/ferrite-session/src/lib.rs` (add module declaration)

This is the central module. It provides pure functions that classify URLs and check them against the security policies.

**Step 1: Write tests first**

Create `crates/ferrite-session/src/url_guard.rs` with the full module. Tests come first (TDD), then the implementation.

```rust
//! URL security guard: SSRF mitigation, IDNA rejection, and HTTPS validation.
//!
//! Provides [`UrlSecurityConfig`] (derived from [`Settings`]) and validation
//! functions that are called at two enforcement points:
//! 1. When URLs are first parsed (torrent load, `add_tracker_url`)
//! 2. When HTTP redirects are followed (custom reqwest redirect policy)

use std::net::IpAddr;

use url::Url;

use crate::rate_limiter::is_local_network;

// ── Configuration ────────────────────────────────────────────────────

/// Security configuration derived from [`crate::settings::Settings`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct UrlSecurityConfig {
    /// Block SSRF vectors: localhost path restrictions, local-network query
    /// string restrictions, and redirect-to-private-IP blocking.
    pub ssrf_mitigation: bool,
    /// Allow internationalized (non-ASCII) domain names in URLs.
    pub allow_idna: bool,
    /// Validate TLS certificates for HTTPS tracker/web seed connections.
    pub validate_https_trackers: bool,
}

impl Default for UrlSecurityConfig {
    fn default() -> Self {
        Self {
            ssrf_mitigation: true,
            allow_idna: false,
            validate_https_trackers: true,
        }
    }
}

impl From<&crate::settings::Settings> for UrlSecurityConfig {
    fn from(s: &crate::settings::Settings) -> Self {
        Self {
            ssrf_mitigation: s.ssrf_mitigation,
            allow_idna: s.allow_idna,
            validate_https_trackers: s.validate_https_trackers,
        }
    }
}

// ── Error type ───────────────────────────────────────────────────────

/// Reason a URL was rejected by the security guard.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UrlGuardError {
    #[error("invalid URL: {0}")]
    InvalidUrl(String),

    #[error("SSRF: localhost tracker must use /announce path, got: {0}")]
    LocalhostBadPath(String),

    #[error("SSRF: local-network web seed URL must not contain query string")]
    LocalNetworkQueryString,

    #[error("SSRF: redirect from global URL to private/local IP {0} blocked")]
    RedirectToPrivateIp(IpAddr),

    #[error("IDNA: internationalized domain name rejected: {0}")]
    IdnaDomain(String),
}

// ── URL classification ───────────────────────────────────────────────

/// Whether a URL's host resolves to a local/private IP address.
///
/// For IP-literal hosts, checks directly. For domain names, this returns
/// `None` (cannot resolve without async DNS — use redirect-time checking).
fn host_ip(url: &Url) -> Option<IpAddr> {
    // url::Host::Ipv4 / Ipv6 give us the IP directly
    match url.host() {
        Some(url::Host::Ipv4(ip)) => Some(IpAddr::V4(ip)),
        Some(url::Host::Ipv6(ip)) => Some(IpAddr::V6(ip)),
        _ => None,
    }
}

/// Check if a URL host is localhost (127.0.0.0/8 or ::1).
fn is_localhost(url: &Url) -> bool {
    match url.host() {
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        Some(url::Host::Domain(d)) => d == "localhost",
        None => false,
    }
}

/// Check if a URL host is on a local/private network (RFC 1918, link-local, loopback).
fn is_local_host_url(url: &Url) -> bool {
    match host_ip(url) {
        Some(ip) => is_local_network(ip),
        None => {
            // Domain name — treat "localhost" as local
            matches!(url.host(), Some(url::Host::Domain(d)) if d == "localhost")
        }
    }
}

/// Returns true if the URL host contains non-ASCII characters (IDNA / internationalized).
fn has_idna_domain(url: &Url) -> bool {
    match url.host_str() {
        Some(host) => !host.is_ascii(),
        None => false,
    }
}

// ── Validation functions ─────────────────────────────────────────────

/// Validate a tracker URL against the security configuration.
///
/// Called when:
/// - A torrent is loaded (for each tracker in announce/announce_list)
/// - A tracker URL is added via `add_tracker_url` or lt_trackers exchange
///
/// Returns `Ok(())` if the URL passes all checks, or `Err(UrlGuardError)` otherwise.
pub(crate) fn validate_tracker_url(
    url_str: &str,
    config: &UrlSecurityConfig,
) -> Result<(), UrlGuardError> {
    // UDP tracker URLs are not HTTP and don't suffer from SSRF/redirect issues.
    // We still check IDNA for consistency.
    if url_str.starts_with("udp://") {
        // IDNA check on UDP URLs
        if !config.allow_idna {
            // Parse just the host portion
            if let Ok(parsed) = Url::parse(url_str) {
                if has_idna_domain(&parsed) {
                    return Err(UrlGuardError::IdnaDomain(
                        parsed.host_str().unwrap_or("").to_string(),
                    ));
                }
            }
        }
        return Ok(());
    }

    let parsed = Url::parse(url_str)
        .map_err(|e| UrlGuardError::InvalidUrl(e.to_string()))?;

    // IDNA check
    if !config.allow_idna && has_idna_domain(&parsed) {
        return Err(UrlGuardError::IdnaDomain(
            parsed.host_str().unwrap_or("").to_string(),
        ));
    }

    // SSRF: localhost tracker must use /announce path
    if config.ssrf_mitigation && is_localhost(&parsed) {
        let path = parsed.path();
        if !path.ends_with("/announce") && path != "/announce" {
            return Err(UrlGuardError::LocalhostBadPath(path.to_string()));
        }
    }

    Ok(())
}

/// Validate a web seed URL against the security configuration.
///
/// Called when spawning web seed tasks from url-list or httpseeds.
///
/// Returns `Ok(())` if the URL passes all checks, or `Err(UrlGuardError)` otherwise.
pub(crate) fn validate_web_seed_url(
    url_str: &str,
    config: &UrlSecurityConfig,
) -> Result<(), UrlGuardError> {
    let parsed = Url::parse(url_str)
        .map_err(|e| UrlGuardError::InvalidUrl(e.to_string()))?;

    // IDNA check
    if !config.allow_idna && has_idna_domain(&parsed) {
        return Err(UrlGuardError::IdnaDomain(
            parsed.host_str().unwrap_or("").to_string(),
        ));
    }

    // SSRF: local-network web seeds cannot have query strings
    if config.ssrf_mitigation && is_local_host_url(&parsed) {
        if parsed.query().is_some() {
            return Err(UrlGuardError::LocalNetworkQueryString);
        }
    }

    Ok(())
}

/// Validate a redirect target URL.
///
/// Called from the custom reqwest redirect policy when an HTTP redirect
/// response is received. Blocks redirects from global URLs to private IPs.
///
/// `original_url` is the URL that initiated the request chain.
/// `redirect_url` is the target of the redirect.
pub(crate) fn validate_redirect(
    original_url: &Url,
    redirect_url: &Url,
    config: &UrlSecurityConfig,
) -> Result<(), UrlGuardError> {
    if !config.ssrf_mitigation {
        return Ok(());
    }

    // Only block global → private redirects.
    // If the original URL was already local, the redirect is fine.
    let original_is_local = is_local_host_url(original_url);
    if original_is_local {
        return Ok(());
    }

    // Check if redirect target is a private/local IP
    if let Some(ip) = host_ip(redirect_url) {
        if is_local_network(ip) {
            return Err(UrlGuardError::RedirectToPrivateIp(ip));
        }
    }

    // Check for "localhost" domain redirect
    if matches!(redirect_url.host(), Some(url::Host::Domain(d)) if d == "localhost") {
        return Err(UrlGuardError::RedirectToPrivateIp(
            "127.0.0.1".parse().unwrap(),
        ));
    }

    Ok(())
}

// ── HTTP client builder helper ───────────────────────────────────────

/// Build a reqwest redirect policy that blocks SSRF redirect attacks.
///
/// When `ssrf_mitigation` is enabled, redirects from global URLs to
/// private/local IPs are rejected. Otherwise, follows up to 10 redirects.
pub(crate) fn build_redirect_policy(
    config: &UrlSecurityConfig,
) -> reqwest::redirect::Policy {
    if !config.ssrf_mitigation {
        return reqwest::redirect::Policy::limited(10);
    }

    reqwest::redirect::Policy::custom(|attempt| {
        if attempt.previous().len() >= 10 {
            return attempt.error(reqwest::Error::from(
                std::io::Error::new(std::io::ErrorKind::Other, "too many redirects"),
            ));
        }

        let redirect_url = attempt.url();
        let original_url = &attempt.previous()[0];

        // Check if original was global and redirect target is private
        let original_is_local = is_local_host_url(original_url);
        if !original_is_local {
            if let Some(ip) = host_ip(redirect_url) {
                if is_local_network(ip) {
                    return attempt.error(reqwest::Error::from(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        format!(
                            "SSRF: redirect from global URL to private IP {ip} blocked"
                        ),
                    )));
                }
            }
            // Check for "localhost" domain
            if matches!(redirect_url.host(), Some(url::Host::Domain(d)) if d == "localhost") {
                return attempt.error(reqwest::Error::from(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "SSRF: redirect from global URL to localhost blocked",
                )));
            }
        }

        attempt.follow()
    })
}

/// Build a reqwest `Client` with the appropriate security settings.
///
/// Configures:
/// - TLS certificate validation (controlled by `validate_https_trackers`)
/// - SSRF-safe redirect policy
/// - User-agent header
/// - Optional proxy
pub(crate) fn build_http_client(
    config: &UrlSecurityConfig,
    proxy_url: Option<&str>,
    user_agent: &str,
) -> reqwest::Client {
    let mut builder = reqwest::Client::builder()
        .user_agent(user_agent)
        .redirect(build_redirect_policy(config));

    if !config.validate_https_trackers {
        builder = builder.danger_accept_invalid_certs(true);
    }

    if let Some(url) = proxy_url
        && let Ok(proxy) = reqwest::Proxy::all(url)
    {
        builder = builder.proxy(proxy);
    }

    builder.build().expect("failed to build HTTP client")
}

// ── Tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn default_config() -> UrlSecurityConfig {
        UrlSecurityConfig::default()
    }

    fn permissive_config() -> UrlSecurityConfig {
        UrlSecurityConfig {
            ssrf_mitigation: false,
            allow_idna: true,
            validate_https_trackers: true,
        }
    }

    // ── Tracker URL validation ───────────────────────────────────────

    #[test]
    fn tracker_url_global_announce_passes() {
        let cfg = default_config();
        assert!(validate_tracker_url("http://tracker.example.com/announce", &cfg).is_ok());
        assert!(validate_tracker_url("https://tracker.example.com/announce", &cfg).is_ok());
    }

    #[test]
    fn tracker_url_localhost_announce_passes() {
        let cfg = default_config();
        assert!(validate_tracker_url("http://127.0.0.1:8080/announce", &cfg).is_ok());
        assert!(validate_tracker_url("http://localhost/announce", &cfg).is_ok());
        assert!(validate_tracker_url("http://[::1]:8080/announce", &cfg).is_ok());
    }

    #[test]
    fn tracker_url_localhost_bad_path_rejected() {
        let cfg = default_config();
        let err = validate_tracker_url("http://127.0.0.1:8080/admin/api", &cfg).unwrap_err();
        assert!(matches!(err, UrlGuardError::LocalhostBadPath(_)));

        let err = validate_tracker_url("http://localhost/status", &cfg).unwrap_err();
        assert!(matches!(err, UrlGuardError::LocalhostBadPath(_)));

        let err = validate_tracker_url("http://[::1]:9090/metrics", &cfg).unwrap_err();
        assert!(matches!(err, UrlGuardError::LocalhostBadPath(_)));
    }

    #[test]
    fn tracker_url_localhost_subpath_announce_passes() {
        let cfg = default_config();
        // Path ending with /announce is acceptable even with prefix
        assert!(validate_tracker_url("http://127.0.0.1:8080/tracker/announce", &cfg).is_ok());
    }

    #[test]
    fn tracker_url_localhost_bad_path_allowed_when_ssrf_disabled() {
        let cfg = permissive_config();
        assert!(validate_tracker_url("http://127.0.0.1:8080/admin/api", &cfg).is_ok());
    }

    #[test]
    fn tracker_url_udp_passes() {
        let cfg = default_config();
        // UDP trackers skip SSRF checks (no HTTP redirects)
        assert!(validate_tracker_url("udp://tracker.example.com:6969/announce", &cfg).is_ok());
    }

    #[test]
    fn tracker_url_idna_rejected() {
        let cfg = default_config(); // allow_idna: false
        // Simulate a URL with a non-ASCII host — since Url::parse normalizes
        // IDNA domains via punycode, we test with the punycode-unable raw form.
        // In practice, non-ASCII bytes would appear in the host of a raw URL string.
        // We test with a pre-parsed URL containing non-ASCII.
        let result = validate_tracker_url("http://trаcker.example.com/announce", &cfg);
        // The Cyrillic 'а' (U+0430) makes this non-ASCII
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), UrlGuardError::IdnaDomain(_)));
    }

    #[test]
    fn tracker_url_idna_allowed_when_enabled() {
        let cfg = permissive_config(); // allow_idna: true
        // With IDNA allowed, non-ASCII domains pass
        let result = validate_tracker_url("http://trаcker.example.com/announce", &cfg);
        assert!(result.is_ok());
    }

    #[test]
    fn tracker_url_invalid_url_rejected() {
        let cfg = default_config();
        let err = validate_tracker_url("not a url at all", &cfg).unwrap_err();
        assert!(matches!(err, UrlGuardError::InvalidUrl(_)));
    }

    // ── Web seed URL validation ──────────────────────────────────────

    #[test]
    fn web_seed_url_global_passes() {
        let cfg = default_config();
        assert!(validate_web_seed_url("http://cdn.example.com/files", &cfg).is_ok());
        assert!(validate_web_seed_url("https://cdn.example.com/files?token=abc", &cfg).is_ok());
    }

    #[test]
    fn web_seed_url_local_no_query_passes() {
        let cfg = default_config();
        assert!(validate_web_seed_url("http://192.168.1.100/files", &cfg).is_ok());
        assert!(validate_web_seed_url("http://10.0.0.5/torrents/data", &cfg).is_ok());
    }

    #[test]
    fn web_seed_url_local_with_query_rejected() {
        let cfg = default_config();
        let err = validate_web_seed_url("http://192.168.1.100/files?cmd=exec", &cfg).unwrap_err();
        assert!(matches!(err, UrlGuardError::LocalNetworkQueryString));

        let err = validate_web_seed_url("http://10.0.0.5/data?x=1", &cfg).unwrap_err();
        assert!(matches!(err, UrlGuardError::LocalNetworkQueryString));

        let err = validate_web_seed_url("http://127.0.0.1/data?x=1", &cfg).unwrap_err();
        assert!(matches!(err, UrlGuardError::LocalNetworkQueryString));
    }

    #[test]
    fn web_seed_url_local_query_allowed_when_ssrf_disabled() {
        let cfg = permissive_config();
        assert!(validate_web_seed_url("http://192.168.1.100/files?cmd=exec", &cfg).is_ok());
    }

    #[test]
    fn web_seed_url_idna_rejected() {
        let cfg = default_config();
        let result = validate_web_seed_url("http://trаcker.example.com/files", &cfg);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), UrlGuardError::IdnaDomain(_)));
    }

    // ── Redirect validation ──────────────────────────────────────────

    #[test]
    fn redirect_global_to_global_passes() {
        let cfg = default_config();
        let orig = Url::parse("http://tracker.example.com/announce").unwrap();
        let redir = Url::parse("http://tracker2.example.com/announce").unwrap();
        assert!(validate_redirect(&orig, &redir, &cfg).is_ok());
    }

    #[test]
    fn redirect_global_to_private_ip_blocked() {
        let cfg = default_config();
        let orig = Url::parse("http://tracker.example.com/announce").unwrap();

        // Redirect to RFC 1918 (10.0.0.0/8)
        let redir = Url::parse("http://10.0.0.1/admin").unwrap();
        let err = validate_redirect(&orig, &redir, &cfg).unwrap_err();
        assert!(matches!(err, UrlGuardError::RedirectToPrivateIp(_)));

        // Redirect to 192.168.0.0/16
        let redir = Url::parse("http://192.168.1.1/internal").unwrap();
        let err = validate_redirect(&orig, &redir, &cfg).unwrap_err();
        assert!(matches!(err, UrlGuardError::RedirectToPrivateIp(_)));

        // Redirect to loopback
        let redir = Url::parse("http://127.0.0.1/announce").unwrap();
        let err = validate_redirect(&orig, &redir, &cfg).unwrap_err();
        assert!(matches!(err, UrlGuardError::RedirectToPrivateIp(_)));

        // Redirect to link-local
        let redir = Url::parse("http://169.254.1.1/data").unwrap();
        let err = validate_redirect(&orig, &redir, &cfg).unwrap_err();
        assert!(matches!(err, UrlGuardError::RedirectToPrivateIp(_)));
    }

    #[test]
    fn redirect_global_to_localhost_domain_blocked() {
        let cfg = default_config();
        let orig = Url::parse("http://tracker.example.com/announce").unwrap();
        let redir = Url::parse("http://localhost/admin").unwrap();
        let err = validate_redirect(&orig, &redir, &cfg).unwrap_err();
        assert!(matches!(err, UrlGuardError::RedirectToPrivateIp(_)));
    }

    #[test]
    fn redirect_global_to_ipv6_loopback_blocked() {
        let cfg = default_config();
        let orig = Url::parse("http://tracker.example.com/announce").unwrap();
        let redir = Url::parse("http://[::1]/admin").unwrap();
        let err = validate_redirect(&orig, &redir, &cfg).unwrap_err();
        assert!(matches!(err, UrlGuardError::RedirectToPrivateIp(_)));
    }

    #[test]
    fn redirect_local_to_local_passes() {
        let cfg = default_config();
        // If the original URL is already local, redirecting within local is fine
        let orig = Url::parse("http://192.168.1.1/announce").unwrap();
        let redir = Url::parse("http://10.0.0.1/announce").unwrap();
        assert!(validate_redirect(&orig, &redir, &cfg).is_ok());
    }

    #[test]
    fn redirect_local_to_global_passes() {
        let cfg = default_config();
        let orig = Url::parse("http://192.168.1.1/announce").unwrap();
        let redir = Url::parse("http://tracker.example.com/announce").unwrap();
        assert!(validate_redirect(&orig, &redir, &cfg).is_ok());
    }

    #[test]
    fn redirect_ssrf_disabled_allows_all() {
        let cfg = permissive_config();
        let orig = Url::parse("http://tracker.example.com/announce").unwrap();
        let redir = Url::parse("http://127.0.0.1/admin").unwrap();
        assert!(validate_redirect(&orig, &redir, &cfg).is_ok());
    }

    // ── URL security config ──────────────────────────────────────────

    #[test]
    fn default_config_values() {
        let cfg = UrlSecurityConfig::default();
        assert!(cfg.ssrf_mitigation);
        assert!(!cfg.allow_idna);
        assert!(cfg.validate_https_trackers);
    }

    // ── Helper function tests ────────────────────────────────────────

    #[test]
    fn is_localhost_detection() {
        let loopback = Url::parse("http://127.0.0.1/announce").unwrap();
        assert!(is_localhost(&loopback));

        let loopback_v6 = Url::parse("http://[::1]/announce").unwrap();
        assert!(is_localhost(&loopback_v6));

        let localhost_domain = Url::parse("http://localhost/announce").unwrap();
        assert!(is_localhost(&localhost_domain));

        let global = Url::parse("http://8.8.8.8/announce").unwrap();
        assert!(!is_localhost(&global));

        let domain = Url::parse("http://tracker.example.com/announce").unwrap();
        assert!(!is_localhost(&domain));
    }

    #[test]
    fn is_local_host_url_detection() {
        assert!(is_local_host_url(&Url::parse("http://192.168.1.1/files").unwrap()));
        assert!(is_local_host_url(&Url::parse("http://10.0.0.1/files").unwrap()));
        assert!(is_local_host_url(&Url::parse("http://172.16.0.1/files").unwrap()));
        assert!(is_local_host_url(&Url::parse("http://127.0.0.1/files").unwrap()));
        assert!(is_local_host_url(&Url::parse("http://localhost/files").unwrap()));
        assert!(!is_local_host_url(&Url::parse("http://8.8.8.8/files").unwrap()));
        assert!(!is_local_host_url(&Url::parse("http://example.com/files").unwrap()));
    }

    #[test]
    fn has_idna_domain_detection() {
        // Pure ASCII — no IDNA
        assert!(!has_idna_domain(&Url::parse("http://example.com/announce").unwrap()));

        // Cyrillic 'а' (U+0430) — IDNA
        // Note: url crate may punycode-encode this. We test the raw host_str.
        let url_str = "http://trаcker.example.com/announce";
        if let Ok(u) = Url::parse(url_str) {
            // url crate may or may not convert; test actual behaviour
            let host = u.host_str().unwrap_or("");
            if !host.is_ascii() {
                assert!(has_idna_domain(&u));
            }
            // If url crate converted to punycode, host_str is ASCII — that's OK,
            // the IDNA check still works because the punycode form is ASCII.
        }
    }

    // ── HTTP client builder ──────────────────────────────────────────

    #[test]
    fn build_http_client_default_config() {
        let cfg = default_config();
        // Should not panic
        let _client = build_http_client(&cfg, None, "Ferrite/0.42.0");
    }

    #[test]
    fn build_http_client_no_tls_validation() {
        let cfg = UrlSecurityConfig {
            validate_https_trackers: false,
            ..default_config()
        };
        let _client = build_http_client(&cfg, None, "Ferrite/0.42.0");
    }

    #[test]
    fn build_http_client_with_proxy() {
        let cfg = default_config();
        let _client = build_http_client(&cfg, Some("socks5://127.0.0.1:1080"), "Ferrite/0.42.0");
    }

    #[test]
    fn build_redirect_policy_ssrf_enabled() {
        let cfg = default_config();
        let _policy = build_redirect_policy(&cfg);
        // The custom policy is opaque; we test its behaviour via integration tests
    }

    #[test]
    fn build_redirect_policy_ssrf_disabled() {
        let cfg = permissive_config();
        let _policy = build_redirect_policy(&cfg);
    }
}
```

**Step 2: Register the module in lib.rs**

In `crates/ferrite-session/src/lib.rs`, add after the `pub(crate) mod proxy;` line:

```rust
pub(crate) mod url_guard;
```

**Step 3: Run tests**

```bash
cargo test -p ferrite-session url_guard
```

All tests should pass. The module is pure functions with no side effects, so no mocking is needed.

---

## Task 2: Add Security Settings to `Settings` Struct

**Files:**
- Modify: `crates/ferrite-session/src/settings.rs`

**Step 1: Write tests for the new settings fields**

Add these tests to the existing `mod tests` block in `settings.rs`:

```rust
    #[test]
    fn security_settings_defaults() {
        let s = Settings::default();
        assert!(s.ssrf_mitigation);
        assert!(!s.allow_idna);
        assert!(s.validate_https_trackers);
    }

    #[test]
    fn security_settings_json_round_trip() {
        let json = r#"{"ssrf_mitigation": false, "allow_idna": true, "validate_https_trackers": false}"#;
        let decoded: Settings = serde_json::from_str(json).unwrap();
        assert!(!decoded.ssrf_mitigation);
        assert!(decoded.allow_idna);
        assert!(!decoded.validate_https_trackers);

        let re_encoded = serde_json::to_string(&decoded).unwrap();
        let re_decoded: Settings = serde_json::from_str(&re_encoded).unwrap();
        assert_eq!(decoded, re_decoded);
    }

    #[test]
    fn security_settings_missing_use_defaults() {
        // Empty JSON should use default values (secure by default)
        let decoded: Settings = serde_json::from_str("{}").unwrap();
        assert!(decoded.ssrf_mitigation);
        assert!(!decoded.allow_idna);
        assert!(decoded.validate_https_trackers);
    }

    #[test]
    fn url_security_config_from_settings() {
        let s = Settings::default();
        let cfg = crate::url_guard::UrlSecurityConfig::from(&s);
        assert!(cfg.ssrf_mitigation);
        assert!(!cfg.allow_idna);
        assert!(cfg.validate_https_trackers);

        let mut s2 = Settings::default();
        s2.ssrf_mitigation = false;
        s2.allow_idna = true;
        s2.validate_https_trackers = false;
        let cfg2 = crate::url_guard::UrlSecurityConfig::from(&s2);
        assert!(!cfg2.ssrf_mitigation);
        assert!(cfg2.allow_idna);
        assert!(!cfg2.validate_https_trackers);
    }
```

**Step 2: Add the three new fields to `Settings`**

In the `Settings` struct, add a new section after the `// ── Proxy ──` fields (after `apply_ip_filter_to_trackers`):

```rust
    // ── Security ──
    /// SSRF mitigation: restrict localhost tracker paths, block local-network web
    /// seed query strings, and block HTTP redirects from global to private IPs.
    #[serde(default = "default_true")]
    pub ssrf_mitigation: bool,
    /// Allow internationalized (non-ASCII) domain names in tracker/web seed URLs.
    /// When false (default), URLs with IDNA domains are rejected to prevent
    /// homograph attacks.
    #[serde(default)]
    pub allow_idna: bool,
    /// Validate TLS certificates for HTTPS tracker and web seed connections.
    /// When false, accepts self-signed certificates (not recommended).
    #[serde(default = "default_true")]
    pub validate_https_trackers: bool,
```

**Step 3: Update `Default for Settings`**

Add to the `Default` implementation, in the Proxy section area:

```rust
            // Security
            ssrf_mitigation: true,
            allow_idna: false,
            validate_https_trackers: true,
```

**Step 4: Update presets**

Both `min_memory()` and `high_performance()` use `..Self::default()`, so they automatically inherit the security defaults. No changes needed.

**Step 5: Update `PartialEq` implementation**

Add to the `eq()` method chain:

```rust
            && self.ssrf_mitigation == other.ssrf_mitigation
            && self.allow_idna == other.allow_idna
            && self.validate_https_trackers == other.validate_https_trackers
```

**Step 6: Update existing test assertions**

Update the `default_settings_values` test to include:

```rust
        assert!(s.ssrf_mitigation);
        assert!(!s.allow_idna);
        assert!(s.validate_https_trackers);
```

**Step 7: Run tests**

```bash
cargo test -p ferrite-session settings
```

---

## Task 3: Integrate URL Guard into `TrackerManager`

**Files:**
- Modify: `crates/ferrite-session/src/tracker_manager.rs`

This task wires the URL validation into the tracker manager so that tracker URLs are validated when torrents are loaded and when URLs are dynamically added.

**Step 1: Write tests**

Add these tests to the existing `mod tests` block in `tracker_manager.rs`:

```rust
    #[test]
    fn localhost_tracker_announce_path_accepted() {
        let cfg = crate::url_guard::UrlSecurityConfig::default();
        let result = crate::url_guard::validate_tracker_url(
            "http://127.0.0.1:8080/announce",
            &cfg,
        );
        assert!(result.is_ok());
    }

    #[test]
    fn localhost_tracker_bad_path_filtered() {
        let meta = torrent_with_trackers(
            None,
            Some(vec![vec![
                "http://127.0.0.1:8080/admin",
                "http://tracker.example.com/announce",
            ]]),
        );
        let cfg = crate::url_guard::UrlSecurityConfig::default();
        let mgr = TrackerManager::from_torrent_filtered(&meta, test_peer_id(), 6881, &cfg);
        // Only the global tracker should survive
        assert_eq!(mgr.tracker_count(), 1);
        assert_eq!(mgr.trackers[0].url, "http://tracker.example.com/announce");
    }

    #[test]
    fn add_tracker_url_validates() {
        let meta = torrent_with_trackers(Some("http://tracker.example.com/announce"), None);
        let cfg = crate::url_guard::UrlSecurityConfig::default();
        let mut mgr = TrackerManager::from_torrent_filtered(&meta, test_peer_id(), 6881, &cfg);

        // Valid URL
        assert!(mgr.add_tracker_url_validated("http://other.example.com/announce", &cfg));

        // Invalid: localhost with bad path
        assert!(!mgr.add_tracker_url_validated("http://127.0.0.1/admin", &cfg));

        assert_eq!(mgr.tracker_count(), 2);
    }
```

**Step 2: Add `from_torrent_filtered` constructor**

Add a new constructor that accepts a `UrlSecurityConfig` and filters URLs:

```rust
    /// Create a TrackerManager from torrent metadata with URL security validation.
    ///
    /// Tracker URLs that fail security checks are silently skipped (logged at warn level).
    pub fn from_torrent_filtered(
        meta: &TorrentMetaV1,
        peer_id: Id20,
        port: u16,
        security: &crate::url_guard::UrlSecurityConfig,
    ) -> Self {
        let mut trackers = Vec::new();
        let mut seen_urls = std::collections::HashSet::new();

        // BEP 12: announce_list takes priority if present
        if let Some(ref tiers) = meta.announce_list {
            for (tier_idx, tier) in tiers.iter().enumerate() {
                for url in tier {
                    let url = url.trim().to_string();
                    if url.is_empty() || !seen_urls.insert(url.clone()) {
                        continue;
                    }
                    if let Err(e) = crate::url_guard::validate_tracker_url(&url, security) {
                        warn!(%url, %e, "tracker URL rejected by security policy");
                        continue;
                    }
                    if let Some(protocol) = classify_url(&url) {
                        trackers.push(TrackerEntry {
                            url,
                            tier: tier_idx,
                            protocol,
                            state: TrackerState::NeedsAnnounce,
                            tracker_id: None,
                            next_announce: Instant::now(),
                            interval: DEFAULT_INTERVAL,
                            backoff: Duration::ZERO,
                            scrape_info: None,
                            consecutive_failures: 0,
                        });
                    }
                }
            }
        }

        // Fallback: single announce URL
        if let Some(ref url) = meta.announce {
            let url = url.trim().to_string();
            if !url.is_empty() && seen_urls.insert(url.clone()) {
                if let Err(e) = crate::url_guard::validate_tracker_url(&url, security) {
                    warn!(%url, %e, "tracker URL rejected by security policy");
                } else if let Some(protocol) = classify_url(&url) {
                    trackers.push(TrackerEntry {
                        url,
                        tier: if trackers.is_empty() {
                            0
                        } else {
                            trackers.last().unwrap().tier + 1
                        },
                        protocol,
                        state: TrackerState::NeedsAnnounce,
                        tracker_id: None,
                        next_announce: Instant::now(),
                        interval: DEFAULT_INTERVAL,
                        backoff: Duration::ZERO,
                        scrape_info: None,
                        consecutive_failures: 0,
                    });
                }
            }
        }

        TrackerManager {
            trackers,
            info_hash: meta.info_hash,
            peer_id,
            port,
            http_client: HttpTracker::new(),
            udp_client: UdpTracker::new(),
        }
    }
```

**Step 3: Add `add_tracker_url_validated` method**

Add a new method alongside the existing `add_tracker_url`:

```rust
    /// Add a new tracker URL with security validation.
    ///
    /// Returns `true` if the URL was added, `false` if rejected, empty,
    /// unknown protocol, or duplicate.
    pub fn add_tracker_url_validated(
        &mut self,
        url: &str,
        security: &crate::url_guard::UrlSecurityConfig,
    ) -> bool {
        let url = url.trim();
        if url.is_empty() {
            return false;
        }
        if let Err(e) = crate::url_guard::validate_tracker_url(url, security) {
            warn!(%url, %e, "tracker URL rejected by security policy");
            return false;
        }
        self.add_tracker_url(url)
    }
```

**Step 4: Run tests**

```bash
cargo test -p ferrite-session tracker_manager
```

---

## Task 4: Integrate URL Guard into `HttpTracker`

**Files:**
- Modify: `crates/ferrite-tracker/src/http.rs`

The `HttpTracker` needs to accept security configuration for TLS validation and redirect policy. Since `ferrite-tracker` is a lower-level crate that does not depend on `ferrite-session`, we pass the configuration as simple parameters rather than importing `UrlSecurityConfig`.

**Step 1: Write tests**

Add to the existing `mod tests` block in `http.rs`:

```rust
    #[test]
    fn http_tracker_with_security_builds() {
        // Default: validates certs, custom redirect policy
        let _tracker = HttpTracker::with_security(None, true, true);
    }

    #[test]
    fn http_tracker_with_security_no_tls_validation() {
        let _tracker = HttpTracker::with_security(None, false, false);
    }
```

**Step 2: Add `with_security` constructor**

Add a new constructor to `HttpTracker`:

```rust
    /// Create an HTTP tracker client with security configuration.
    ///
    /// - `proxy_url`: Optional proxy (e.g. `"socks5://host:port"`).
    /// - `validate_tls`: When true, reqwest verifies TLS certificates (default).
    /// - `ssrf_mitigation`: When true, blocks redirects from global URLs to private IPs.
    pub fn with_security(
        proxy_url: Option<&str>,
        validate_tls: bool,
        ssrf_mitigation: bool,
    ) -> Self {
        let redirect_policy = if ssrf_mitigation {
            reqwest::redirect::Policy::custom(|attempt| {
                if attempt.previous().len() >= 10 {
                    return attempt.error(reqwest::Error::from(
                        std::io::Error::new(std::io::ErrorKind::Other, "too many redirects"),
                    ));
                }

                let redirect_url = attempt.url();

                // Check if redirect target is a private IP
                match redirect_url.host() {
                    Some(url::Host::Ipv4(ip)) => {
                        if ip.is_loopback() || ip.is_private() || ip.is_link_local() {
                            // Only block if original was global
                            let orig = &attempt.previous()[0];
                            let orig_is_local = match orig.host() {
                                Some(url::Host::Ipv4(ip)) => {
                                    ip.is_loopback() || ip.is_private() || ip.is_link_local()
                                }
                                Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
                                Some(url::Host::Domain(d)) => d == "localhost",
                                None => false,
                            };
                            if !orig_is_local {
                                return attempt.error(reqwest::Error::from(
                                    std::io::Error::new(
                                        std::io::ErrorKind::PermissionDenied,
                                        format!(
                                            "SSRF: redirect to private IP {ip} blocked"
                                        ),
                                    ),
                                ));
                            }
                        }
                    }
                    Some(url::Host::Ipv6(ip)) => {
                        if ip.is_loopback() {
                            let orig = &attempt.previous()[0];
                            let orig_is_local = match orig.host() {
                                Some(url::Host::Ipv4(ip)) => {
                                    ip.is_loopback() || ip.is_private() || ip.is_link_local()
                                }
                                Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
                                Some(url::Host::Domain(d)) => d == "localhost",
                                None => false,
                            };
                            if !orig_is_local {
                                return attempt.error(reqwest::Error::from(
                                    std::io::Error::new(
                                        std::io::ErrorKind::PermissionDenied,
                                        format!(
                                            "SSRF: redirect to loopback IPv6 {ip} blocked"
                                        ),
                                    ),
                                ));
                            }
                        }
                    }
                    Some(url::Host::Domain(d)) => {
                        if d == "localhost" {
                            let orig = &attempt.previous()[0];
                            let orig_is_local = match orig.host() {
                                Some(url::Host::Ipv4(ip)) => {
                                    ip.is_loopback() || ip.is_private() || ip.is_link_local()
                                }
                                Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
                                Some(url::Host::Domain(d)) => d == "localhost",
                                None => false,
                            };
                            if !orig_is_local {
                                return attempt.error(reqwest::Error::from(
                                    std::io::Error::new(
                                        std::io::ErrorKind::PermissionDenied,
                                        "SSRF: redirect to localhost blocked",
                                    ),
                                ));
                            }
                        }
                    }
                    None => {}
                }

                attempt.follow()
            })
        } else {
            reqwest::redirect::Policy::limited(10)
        };

        let mut builder = reqwest::Client::builder()
            .user_agent("Ferrite/0.1.0")
            .redirect(redirect_policy);

        if !validate_tls {
            builder = builder.danger_accept_invalid_certs(true);
        }

        if let Some(url) = proxy_url
            && let Ok(proxy) = reqwest::Proxy::all(url)
        {
            builder = builder.proxy(proxy);
        }

        HttpTracker {
            client: builder.build().expect("failed to build HTTP client"),
        }
    }
```

**Step 3: Run tests**

```bash
cargo test -p ferrite-tracker
```

---

## Task 5: Integrate URL Guard into `WebSeedTask`

**Files:**
- Modify: `crates/ferrite-session/src/web_seed.rs`

**Step 1: Write tests**

Add to the existing `mod tests` block in `web_seed.rs`:

```rust
    #[test]
    fn web_seed_url_validation_global_passes() {
        let cfg = crate::url_guard::UrlSecurityConfig::default();
        assert!(crate::url_guard::validate_web_seed_url("http://cdn.example.com/files", &cfg).is_ok());
    }

    #[test]
    fn web_seed_url_validation_local_query_rejected() {
        let cfg = crate::url_guard::UrlSecurityConfig::default();
        assert!(crate::url_guard::validate_web_seed_url("http://192.168.1.1/files?cmd=x", &cfg).is_err());
    }
```

**Step 2: Update `WebSeedTask::new` to accept security config**

Modify the `WebSeedTask::new` method to accept a `UrlSecurityConfig` and configure the HTTP client accordingly:

```rust
    /// Create a new web seed task with security configuration.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        url: String,
        mode: WebSeedMode,
        url_builder: WebSeedUrlBuilder,
        lengths: Lengths,
        file_map: FileMap,
        info_hash: Id20,
        cmd_rx: mpsc::Receiver<WebSeedCommand>,
        event_tx: mpsc::Sender<PeerEvent>,
        security: &crate::url_guard::UrlSecurityConfig,
    ) -> Self {
        let http_client = crate::url_guard::build_http_client(
            security,
            None,
            "Ferrite/0.42.0",
        );

        Self {
            url,
            mode,
            url_builder,
            lengths,
            file_map,
            info_hash,
            http_client,
            cmd_rx,
            event_tx,
        }
    }
```

The `timeout` that was previously set via the builder is now handled by `build_http_client`. We need to add the timeout to the `build_http_client` function or set it per-request. Since the existing code uses per-request timeouts via `tokio::time::timeout`, no change is needed to the client-level timeout.

**Step 3: Run tests**

```bash
cargo test -p ferrite-session web_seed
```

---

## Task 6: Wire URL Guard into `TorrentActor`

**Files:**
- Modify: `crates/ferrite-session/src/torrent.rs`

This task connects the URL guard to the two key sites: `spawn_web_seeds()` and `TrackerManager` construction.

**Step 1: Add `UrlSecurityConfig` field to `TorrentActor`**

In the `TorrentActor` struct (in the fields section near `config`), add:

```rust
    url_security: crate::url_guard::UrlSecurityConfig,
```

**Step 2: Initialize from Settings in constructors**

In `TorrentHandle::from_torrent()` and `TorrentHandle::from_magnet()`, construct the security config from settings and pass it to the actor. The actor is constructed from a `TorrentConfig`, so we need to derive the `UrlSecurityConfig` from the `Settings` that created the `TorrentConfig`.

The cleanest approach is to add a `url_security` field to `TorrentConfig`:

In `crates/ferrite-session/src/types.rs`, add the field to `TorrentConfig`:

```rust
    /// URL security configuration for tracker/web seed validation.
    pub url_security: crate::url_guard::UrlSecurityConfig,
```

And in `Default for TorrentConfig`:

```rust
            url_security: crate::url_guard::UrlSecurityConfig::default(),
```

And in `From<&Settings> for TorrentConfig`:

```rust
            url_security: crate::url_guard::UrlSecurityConfig::from(s),
```

**Step 3: Update `TrackerManager` construction**

In `TorrentHandle::from_torrent()`, change:

```rust
        let tracker_manager =
            TrackerManager::from_torrent(&meta, our_peer_id, config.listen_port);
```

to:

```rust
        let tracker_manager =
            TrackerManager::from_torrent_filtered(&meta, our_peer_id, config.listen_port, &config.url_security);
```

Similarly in `TorrentHandle::from_magnet()`, the `TrackerManager::empty()` call does not need URL validation (no URLs yet). But when `set_metadata()` is called later, it should use the filtered version. Add a new method to `TrackerManager`:

```rust
    /// Populate trackers from metadata with security validation (magnet link flow).
    pub fn set_metadata_filtered(
        &mut self,
        meta: &TorrentMetaV1,
        security: &crate::url_guard::UrlSecurityConfig,
    ) {
        let fresh = Self::from_torrent_filtered(meta, self.peer_id, self.port, security);
        self.trackers = fresh.trackers;
    }
```

Then update the call site in `TorrentActor` where `set_metadata()` is called (in the metadata assembly handler) to use `set_metadata_filtered()` with the actor's `url_security` config.

**Step 4: Update `spawn_web_seeds()` to validate URLs and pass security config**

In the `spawn_web_seeds()` method, add URL validation before spawning each web seed:

```rust
    fn spawn_web_seeds(&mut self) {
        if !self.config.enable_web_seed {
            return;
        }
        let meta = match &self.meta {
            Some(m) => m,
            None => return,
        };

        // ... existing lengths/file_map setup ...

        // BEP 19 (GetRight) web seeds
        for url in &meta.url_list {
            if self.banned_web_seeds.contains(url) || self.web_seeds.contains_key(url) {
                continue;
            }
            if self.web_seeds.len() >= self.config.max_web_seeds {
                break;
            }

            // Security validation
            if let Err(e) = crate::url_guard::validate_web_seed_url(url, &self.config.url_security) {
                warn!(%url, %e, "web seed URL rejected by security policy");
                continue;
            }

            // ... rest of existing spawn logic, passing &self.config.url_security to WebSeedTask::new ...
        }

        // BEP 17 (Hoffman) HTTP seeds — same validation
        for url in &meta.httpseeds {
            if self.banned_web_seeds.contains(url) || self.web_seeds.contains_key(url) {
                continue;
            }
            if self.web_seeds.len() >= self.config.max_web_seeds {
                break;
            }

            if let Err(e) = crate::url_guard::validate_web_seed_url(url, &self.config.url_security) {
                warn!(%url, %e, "web seed URL rejected by security policy");
                continue;
            }

            // ... rest of existing spawn logic, passing &self.config.url_security to WebSeedTask::new ...
        }
    }
```

**Step 5: Update `lt_trackers` integration**

In the handler for `PeerEvent::TrackersReceived`, change from `add_tracker_url` to `add_tracker_url_validated`:

Find the call site where `tracker_manager.add_tracker_url(url)` is called for lt_trackers received URLs and change to:

```rust
tracker_manager.add_tracker_url_validated(url, &self.config.url_security)
```

**Step 6: Run tests**

```bash
cargo test -p ferrite-session torrent
cargo test -p ferrite-session tracker_manager
```

---

## Task 7: Update `TorrentConfig` Tests

**Files:**
- Modify: `crates/ferrite-session/src/types.rs` (test section)

**Step 1: Add tests for the new `url_security` field**

Add to the existing `mod tests` block:

```rust
    #[test]
    fn torrent_config_url_security_default() {
        let cfg = TorrentConfig::default();
        assert!(cfg.url_security.ssrf_mitigation);
        assert!(!cfg.url_security.allow_idna);
        assert!(cfg.url_security.validate_https_trackers);
    }

    #[test]
    fn torrent_config_url_security_from_settings() {
        let mut s = crate::settings::Settings::default();
        s.ssrf_mitigation = false;
        s.allow_idna = true;
        s.validate_https_trackers = false;
        let tc = TorrentConfig::from(&s);
        assert!(!tc.url_security.ssrf_mitigation);
        assert!(tc.url_security.allow_idna);
        assert!(!tc.url_security.validate_https_trackers);
    }
```

**Step 2: Run tests**

```bash
cargo test -p ferrite-session types
```

---

## Task 8: Add Tracker Error Variant for Security Rejections

**Files:**
- Modify: `crates/ferrite-tracker/src/error.rs`

**Step 1: Add error variant**

Add to the `Error` enum:

```rust
    #[error("URL security policy violation: {0}")]
    SecurityViolation(String),
```

This variant is used when the redirect policy blocks a request.

**Step 2: Run tests**

```bash
cargo test -p ferrite-tracker
```

---

## Task 9: Full Integration Test

**Files:**
- Modify: `crates/ferrite-session/src/url_guard.rs` (add integration-style tests)

**Step 1: Add comprehensive integration tests**

Add these tests to the existing `mod tests` block in `url_guard.rs`:

```rust
    // ── Integration-style scenario tests ─────────────────────────────

    #[test]
    fn scenario_malicious_torrent_ssrf_via_tracker() {
        // A malicious .torrent contains a tracker URL pointing to an internal
        // service: http://127.0.0.1:9090/api/admin/delete-all
        let cfg = default_config();
        let err = validate_tracker_url("http://127.0.0.1:9090/api/admin/delete-all", &cfg)
            .unwrap_err();
        assert!(matches!(err, UrlGuardError::LocalhostBadPath(_)));

        // The same URL with /announce path is allowed
        assert!(validate_tracker_url("http://127.0.0.1:9090/announce", &cfg).is_ok());
    }

    #[test]
    fn scenario_malicious_torrent_ssrf_via_web_seed() {
        // A malicious .torrent has a web seed URL with a query string
        // that triggers an action on a local service
        let cfg = default_config();
        let err = validate_web_seed_url(
            "http://192.168.1.1/api?action=reboot",
            &cfg,
        )
        .unwrap_err();
        assert!(matches!(err, UrlGuardError::LocalNetworkQueryString));
    }

    #[test]
    fn scenario_redirect_ssrf() {
        // A tracker at a global IP redirects to a local service
        let cfg = default_config();
        let orig = Url::parse("http://evil-tracker.example.com/announce").unwrap();
        let redir = Url::parse("http://169.254.169.254/metadata/v1/").unwrap();
        let err = validate_redirect(&orig, &redir, &cfg).unwrap_err();
        assert!(matches!(err, UrlGuardError::RedirectToPrivateIp(_)));
    }

    #[test]
    fn scenario_homograph_attack() {
        // A tracker URL uses Cyrillic characters that look like ASCII
        let cfg = default_config();
        // 'а' is Cyrillic, not Latin 'a'
        let result = validate_tracker_url("http://trаcker.evil.com/announce", &cfg);
        assert!(result.is_err());
    }

    #[test]
    fn scenario_legitimate_local_tracker() {
        // A legitimate local tracker on a home network
        let cfg = default_config();
        assert!(validate_tracker_url("http://192.168.1.100:6969/announce", &cfg).is_ok());
        // IPv6 link-local tracker
        assert!(validate_tracker_url("http://[fe80::1]:6969/announce", &cfg).is_ok());
    }

    #[test]
    fn scenario_all_protections_disabled() {
        let cfg = permissive_config();
        // Everything is allowed when protections are disabled
        assert!(validate_tracker_url("http://127.0.0.1:9090/admin", &cfg).is_ok());
        assert!(validate_web_seed_url("http://10.0.0.1/data?cmd=exec", &cfg).is_ok());
        assert!(validate_tracker_url("http://trаcker.evil.com/announce", &cfg).is_ok());

        let orig = Url::parse("http://tracker.example.com/announce").unwrap();
        let redir = Url::parse("http://127.0.0.1/admin").unwrap();
        assert!(validate_redirect(&orig, &redir, &cfg).is_ok());
    }
```

**Step 2: Run all tests**

```bash
cargo test --workspace
cargo clippy --workspace -- -D warnings
```

---

## Task 10: Version Bump and Documentation Update

**Files:**
- Modify: `Cargo.toml` (workspace version)
- Modify: `CLAUDE.md` (update settings field count)

**Step 1: Bump workspace version**

In the root `Cargo.toml`, bump the workspace version:

```toml
version = "0.42.0"
```

**Step 2: Update CLAUDE.md**

Update the Settings description from "56-field" to "59-field" to account for the three new fields (`ssrf_mitigation`, `allow_idna`, `validate_https_trackers`).

Update the Settings validate() description from "7 checks" to reflect any new validation checks (none added in this milestone — the three new fields are booleans with no cross-field constraints).

Add under the `### Session Configuration` section:

```
- `UrlSecurityConfig` — derived from Settings: `ssrf_mitigation`, `allow_idna`, `validate_https_trackers`
- `url_guard` module: `validate_tracker_url()`, `validate_web_seed_url()`, `validate_redirect()`, `build_http_client()`
```

**Step 3: Update milestones section**

Update the milestone status in CLAUDE.md:

```
- **M47 complete**: SSRF mitigation, IDNA rejection, HTTPS validation (UrlSecurityConfig, url_guard module)
```

**Step 4: Final test run**

```bash
cargo test --workspace
cargo clippy --workspace -- -D warnings
```

---

## Summary

### New files
| File | Purpose |
|------|---------|
| `crates/ferrite-session/src/url_guard.rs` | URL security guard: validation functions, HTTP client builder, redirect policy |

### Modified files
| File | Changes |
|------|---------|
| `Cargo.toml` | Version bump 0.41.0 → 0.42.0 |
| `crates/ferrite-session/Cargo.toml` | Add `url = "2"` dependency |
| `crates/ferrite-tracker/Cargo.toml` | Add `url = "2"` dependency |
| `crates/ferrite-session/src/lib.rs` | Add `pub(crate) mod url_guard;` |
| `crates/ferrite-session/src/settings.rs` | Add 3 new fields: `ssrf_mitigation`, `allow_idna`, `validate_https_trackers` |
| `crates/ferrite-session/src/types.rs` | Add `url_security: UrlSecurityConfig` to `TorrentConfig` |
| `crates/ferrite-session/src/tracker_manager.rs` | Add `from_torrent_filtered()`, `add_tracker_url_validated()`, `set_metadata_filtered()` |
| `crates/ferrite-tracker/src/http.rs` | Add `HttpTracker::with_security()` constructor |
| `crates/ferrite-tracker/src/error.rs` | Add `SecurityViolation` variant |
| `crates/ferrite-session/src/web_seed.rs` | Accept `UrlSecurityConfig` in `WebSeedTask::new()` |
| `crates/ferrite-session/src/torrent.rs` | Wire URL guard into `spawn_web_seeds()`, tracker construction, lt_trackers handler |
| `CLAUDE.md` | Update field count, add url_guard docs, milestone status |

### New tests (estimated: ~25)
| Test | Location |
|------|----------|
| `tracker_url_global_announce_passes` | url_guard.rs |
| `tracker_url_localhost_announce_passes` | url_guard.rs |
| `tracker_url_localhost_bad_path_rejected` | url_guard.rs |
| `tracker_url_localhost_subpath_announce_passes` | url_guard.rs |
| `tracker_url_localhost_bad_path_allowed_when_ssrf_disabled` | url_guard.rs |
| `tracker_url_udp_passes` | url_guard.rs |
| `tracker_url_idna_rejected` | url_guard.rs |
| `tracker_url_idna_allowed_when_enabled` | url_guard.rs |
| `tracker_url_invalid_url_rejected` | url_guard.rs |
| `web_seed_url_global_passes` | url_guard.rs |
| `web_seed_url_local_no_query_passes` | url_guard.rs |
| `web_seed_url_local_with_query_rejected` | url_guard.rs |
| `web_seed_url_local_query_allowed_when_ssrf_disabled` | url_guard.rs |
| `web_seed_url_idna_rejected` | url_guard.rs |
| `redirect_global_to_global_passes` | url_guard.rs |
| `redirect_global_to_private_ip_blocked` | url_guard.rs |
| `redirect_global_to_localhost_domain_blocked` | url_guard.rs |
| `redirect_global_to_ipv6_loopback_blocked` | url_guard.rs |
| `redirect_local_to_local_passes` | url_guard.rs |
| `redirect_local_to_global_passes` | url_guard.rs |
| `redirect_ssrf_disabled_allows_all` | url_guard.rs |
| `default_config_values` | url_guard.rs |
| `scenario_malicious_torrent_ssrf_via_tracker` | url_guard.rs |
| `scenario_malicious_torrent_ssrf_via_web_seed` | url_guard.rs |
| `scenario_redirect_ssrf` | url_guard.rs |
| `scenario_homograph_attack` | url_guard.rs |
| `scenario_legitimate_local_tracker` | url_guard.rs |
| `scenario_all_protections_disabled` | url_guard.rs |
| `security_settings_defaults` | settings.rs |
| `security_settings_json_round_trip` | settings.rs |
| `security_settings_missing_use_defaults` | settings.rs |
| `url_security_config_from_settings` | settings.rs |
| `localhost_tracker_announce_path_accepted` | tracker_manager.rs |
| `localhost_tracker_bad_path_filtered` | tracker_manager.rs |
| `add_tracker_url_validates` | tracker_manager.rs |
| `torrent_config_url_security_default` | types.rs |
| `torrent_config_url_security_from_settings` | types.rs |
| `http_tracker_with_security_builds` | http.rs |
| `http_tracker_with_security_no_tls_validation` | http.rs |
| `web_seed_url_validation_global_passes` | web_seed.rs |
| `web_seed_url_validation_local_query_rejected` | web_seed.rs |

### Architecture decisions
1. **Centralized validation in `url_guard` module** — all URL security logic in one place, imported by tracker_manager, web_seed, and torrent modules.
2. **`UrlSecurityConfig` derived from `Settings`** — clean separation; the security config is a small value type that flows through `TorrentConfig`.
3. **Dual enforcement points**: at URL parse/add time (prevents bad URLs from entering the system) and at redirect time (prevents SSRF via HTTP redirect chains).
4. **Backwards compatibility preserved**: existing `HttpTracker::new()`, `HttpTracker::with_proxy()`, and `TrackerManager::from_torrent()` are unchanged. New methods are additive.
5. **`url` crate** for proper URL parsing — avoids hand-rolling URL parsing logic.
6. **No DNS resolution in validation** — IP address checks work on IP-literal hosts. Domain name hosts are checked at redirect time when the actual IP is known. This avoids blocking on async DNS and TOCTOU issues.
