//! URL security validation — SSRF mitigation, IDNA rejection, HTTPS enforcement.
//!
//! Provides centralized URL checking for tracker announces and web seed requests.
//! Guards against server-side request forgery by rejecting redirects from public
//! to private IP ranges, restricting localhost tracker paths, and optionally
//! rejecting internationalised domain names (IDNA).

use std::net::IpAddr;

use url::Url;

use crate::rate_limiter::is_local_network;

// ── Configuration ─────────────────────────────────────────────────────

/// URL security configuration extracted from [`Settings`](crate::Settings).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UrlSecurityConfig {
    /// Block requests to private/loopback IPs and restrict localhost tracker paths.
    pub ssrf_mitigation: bool,
    /// Allow internationalised (non-ASCII) domain names in URLs.
    pub allow_idna: bool,
    /// Require HTTPS for HTTP tracker announces (UDP trackers are unaffected).
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

// ── Errors ────────────────────────────────────────────────────────────

/// Errors returned by URL validation functions.
#[derive(Debug, thiserror::Error)]
pub enum UrlGuardError {
    #[error("invalid URL: {0}")]
    InvalidUrl(String),

    #[error("SSRF: localhost tracker must use /announce path, got: {0}")]
    LocalhostBadPath(String),

    #[error("local-network web seed URL must not contain a query string")]
    LocalNetworkQueryString,

    #[error("SSRF: redirect from global URL to private/local IP {0} blocked")]
    #[allow(dead_code)] // Used by validate_redirect() which is called from reqwest redirect policies.
    RedirectToPrivateIp(IpAddr),

    #[error("internationalised domain name (IDNA) rejected: {0}")]
    IdnaDomain(String),
}

// ── Private helpers ───────────────────────────────────────────────────

/// Extract an IP address from a URL whose host is an IP literal.
fn host_ip(url: &Url) -> Option<IpAddr> {
    match url.host()? {
        url::Host::Ipv4(ip) => Some(IpAddr::V4(ip)),
        url::Host::Ipv6(ip) => Some(IpAddr::V6(ip)),
        url::Host::Domain(_) => None,
    }
}

/// Returns `true` if the URL points to localhost (127.0.0.0/8, ::1, or "localhost").
fn is_localhost(url: &Url) -> bool {
    match url.host() {
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        Some(url::Host::Domain(d)) => d == "localhost",
        None => false,
    }
}

/// Returns `true` if the URL's host is a local/private network address.
fn is_local_host_url(url: &Url) -> bool {
    match host_ip(url) {
        Some(ip) => is_local_network(ip),
        None => {
            // Domain name — only "localhost" is considered local.
            matches!(url.host_str(), Some("localhost"))
        }
    }
}

/// Returns `true` if the URL contains a non-ASCII (internationalised) domain name.
///
/// The `url` crate may punycode-encode non-ASCII hostnames, so we check the
/// original host string for either non-ASCII characters or the punycode `xn--`
/// prefix in any label.
fn has_idna_domain(url: &Url) -> bool {
    match url.host_str() {
        Some(host) => {
            // Check for non-ASCII characters (direct Unicode representation).
            if !host.is_ascii() {
                return true;
            }
            // Check for punycode-encoded labels (url crate may convert to ASCII).
            host.split('.').any(|label| label.starts_with("xn--"))
        }
        None => false,
    }
}

// ── Public validation functions ───────────────────────────────────────

/// Validate a tracker announce URL.
///
/// - UDP trackers skip SSRF checks (they don't follow HTTP redirects) but
///   still undergo IDNA validation.
/// - HTTP/HTTPS trackers check IDNA + localhost path restrictions.
pub(crate) fn validate_tracker_url(
    url_str: &str,
    config: &UrlSecurityConfig,
) -> Result<(), UrlGuardError> {
    let url = Url::parse(url_str).map_err(|e| UrlGuardError::InvalidUrl(e.to_string()))?;

    // IDNA check applies to all URL schemes.
    if !config.allow_idna && has_idna_domain(&url) {
        return Err(UrlGuardError::IdnaDomain(
            url.host_str().unwrap_or_default().to_string(),
        ));
    }

    // UDP trackers don't need SSRF checks — they can't follow redirects.
    if url.scheme() == "udp" {
        return Ok(());
    }

    // SSRF: localhost tracker URLs must have path ending in /announce.
    if config.ssrf_mitigation && is_localhost(&url) && !url.path().ends_with("/announce") {
        return Err(UrlGuardError::LocalhostBadPath(url.path().to_string()));
    }

    Ok(())
}

/// Validate a web seed (BEP 19 / BEP 17) URL.
///
/// - IDNA check.
/// - Local-network URLs must not have a query string (prevents info leakage).
pub(crate) fn validate_web_seed_url(
    url_str: &str,
    config: &UrlSecurityConfig,
) -> Result<(), UrlGuardError> {
    let url = Url::parse(url_str).map_err(|e| UrlGuardError::InvalidUrl(e.to_string()))?;

    if !config.allow_idna && has_idna_domain(&url) {
        return Err(UrlGuardError::IdnaDomain(
            url.host_str().unwrap_or_default().to_string(),
        ));
    }

    if config.ssrf_mitigation && is_local_host_url(&url) && url.query().is_some() {
        return Err(UrlGuardError::LocalNetworkQueryString);
    }

    Ok(())
}

/// Validate an HTTP redirect target against SSRF policy.
///
/// Blocks redirects from a public (non-local) origin to a private/local IP.
#[allow(dead_code)] // Public API for callers to pre-check redirects; also tested in scenario tests.
pub(crate) fn validate_redirect(
    original_url: &Url,
    redirect_url: &Url,
    config: &UrlSecurityConfig,
) -> Result<(), UrlGuardError> {
    if !config.ssrf_mitigation {
        return Ok(());
    }

    let orig_local = match host_ip(original_url) {
        Some(ip) => is_local_network(ip),
        None => is_localhost(original_url),
    };

    // Only block public -> private redirects; private -> private is fine.
    if orig_local {
        return Ok(());
    }

    let redirect_ip = host_ip(redirect_url);
    let redirect_local = match redirect_ip {
        Some(ip) => is_local_network(ip),
        None => is_localhost(redirect_url),
    };

    if redirect_local {
        let ip = redirect_ip.unwrap_or_else(|| "127.0.0.1".parse().unwrap());
        return Err(UrlGuardError::RedirectToPrivateIp(ip));
    }

    Ok(())
}

// ── HTTP helpers ──────────────────────────────────────────────────────

/// Build a reqwest redirect policy that blocks SSRF redirect attacks.
///
/// If SSRF mitigation is enabled, redirects from public to private IPs are
/// rejected. Otherwise a standard 10-hop redirect policy is used.
pub(crate) fn build_redirect_policy(
    config: &UrlSecurityConfig,
) -> reqwest::redirect::Policy {
    if !config.ssrf_mitigation {
        return reqwest::redirect::Policy::limited(10);
    }

    reqwest::redirect::Policy::custom(move |attempt| {
        if attempt.previous().len() >= 10 {
            return attempt.error(std::io::Error::other("too many redirects"));
        }

        let original = &attempt.previous()[0];
        let redirect = attempt.url();

        let orig_local = match original.host() {
            Some(url::Host::Ipv4(ip)) => is_local_network(IpAddr::V4(ip)),
            Some(url::Host::Ipv6(ip)) => is_local_network(IpAddr::V6(ip)),
            Some(url::Host::Domain(d)) => d == "localhost",
            None => false,
        };

        if !orig_local {
            let redirect_local = match redirect.host() {
                Some(url::Host::Ipv4(ip)) => is_local_network(IpAddr::V4(ip)),
                Some(url::Host::Ipv6(ip)) => is_local_network(IpAddr::V6(ip)),
                Some(url::Host::Domain(d)) => d == "localhost",
                None => false,
            };

            if redirect_local {
                return attempt.error(std::io::Error::other(
                    "redirect from public to private IP blocked (SSRF)",
                ));
            }
        }

        attempt.follow()
    })
}

/// Build a configured reqwest HTTP client with SSRF-safe redirect policy.
pub(crate) fn build_http_client(
    config: &UrlSecurityConfig,
    proxy_url: Option<&str>,
    user_agent: &str,
) -> reqwest::Client {
    let mut builder = reqwest::Client::builder()
        .user_agent(user_agent)
        .redirect(build_redirect_policy(config))
        .timeout(std::time::Duration::from_secs(30))
        .connect_timeout(std::time::Duration::from_secs(10));

    if !config.validate_https_trackers {
        builder = builder.danger_accept_invalid_certs(true);
    }

    if let Some(proxy) = proxy_url
        && let Ok(p) = reqwest::Proxy::all(proxy)
    {
        builder = builder.proxy(p);
    }

    builder.build().expect("failed to build HTTP client")
}

// ── Tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn ssrf_config() -> UrlSecurityConfig {
        UrlSecurityConfig {
            ssrf_mitigation: true,
            allow_idna: false,
            validate_https_trackers: true,
        }
    }

    fn permissive_config() -> UrlSecurityConfig {
        UrlSecurityConfig {
            ssrf_mitigation: false,
            allow_idna: true,
            validate_https_trackers: false,
        }
    }

    // ── Config defaults ──

    #[test]
    fn url_security_config_defaults() {
        let cfg = UrlSecurityConfig::default();
        assert!(cfg.ssrf_mitigation);
        assert!(!cfg.allow_idna);
        assert!(cfg.validate_https_trackers);
    }

    // ── Helper functions ──

    #[test]
    fn host_ip_extraction() {
        let url = Url::parse("http://192.168.1.1:8080/path").unwrap();
        assert_eq!(host_ip(&url), Some("192.168.1.1".parse().unwrap()));

        let url = Url::parse("http://[::1]:8080/path").unwrap();
        assert_eq!(host_ip(&url), Some("::1".parse().unwrap()));

        let url = Url::parse("http://example.com/path").unwrap();
        assert_eq!(host_ip(&url), None);
    }

    #[test]
    fn localhost_detection() {
        assert!(is_localhost(&Url::parse("http://127.0.0.1/announce").unwrap()));
        assert!(is_localhost(&Url::parse("http://127.0.0.5:8080/announce").unwrap()));
        assert!(is_localhost(&Url::parse("http://[::1]/announce").unwrap()));
        assert!(is_localhost(&Url::parse("http://localhost/announce").unwrap()));
        assert!(!is_localhost(&Url::parse("http://10.0.0.1/announce").unwrap()));
        assert!(!is_localhost(&Url::parse("http://example.com/announce").unwrap()));
    }

    #[test]
    fn idna_domain_detection() {
        // The url crate punycode-encodes non-ASCII domains, so we check for xn-- labels.
        let url = Url::parse("http://xn--nxasmq6b.example.com/path").unwrap();
        assert!(has_idna_domain(&url));

        // Plain ASCII domain should not be flagged.
        let url = Url::parse("http://tracker.example.com/announce").unwrap();
        assert!(!has_idna_domain(&url));

        // IP-literal host: no domain, no IDNA.
        let url = Url::parse("http://192.168.1.1/path").unwrap();
        assert!(!has_idna_domain(&url));
    }

    // ── Tracker URL validation ──

    #[test]
    fn tracker_url_valid_public_http() {
        let cfg = ssrf_config();
        assert!(validate_tracker_url("http://tracker.example.com/announce", &cfg).is_ok());
        assert!(validate_tracker_url("https://tracker.example.com/announce", &cfg).is_ok());
    }

    #[test]
    fn tracker_url_valid_udp() {
        let cfg = ssrf_config();
        assert!(validate_tracker_url("udp://tracker.example.com:6969/announce", &cfg).is_ok());
    }

    #[test]
    fn tracker_url_udp_localhost_allowed() {
        // UDP trackers skip SSRF checks entirely.
        let cfg = ssrf_config();
        assert!(validate_tracker_url("udp://127.0.0.1:6969/announce", &cfg).is_ok());
        assert!(validate_tracker_url("udp://127.0.0.1:6969/bad/path", &cfg).is_ok());
    }

    #[test]
    fn tracker_url_localhost_good_path() {
        let cfg = ssrf_config();
        assert!(validate_tracker_url("http://127.0.0.1:8080/announce", &cfg).is_ok());
        assert!(validate_tracker_url("http://localhost/announce", &cfg).is_ok());
        assert!(validate_tracker_url("http://127.0.0.1/custom/announce", &cfg).is_ok());
    }

    #[test]
    fn tracker_url_localhost_bad_path() {
        let cfg = ssrf_config();
        assert!(matches!(
            validate_tracker_url("http://127.0.0.1:8080/api/admin", &cfg),
            Err(UrlGuardError::LocalhostBadPath(_))
        ));
        assert!(matches!(
            validate_tracker_url("http://localhost/", &cfg),
            Err(UrlGuardError::LocalhostBadPath(_))
        ));
    }

    #[test]
    fn tracker_url_localhost_ssrf_disabled() {
        let mut cfg = ssrf_config();
        cfg.ssrf_mitigation = false;
        // With SSRF disabled, bad paths on localhost are allowed.
        assert!(validate_tracker_url("http://127.0.0.1:8080/api/admin", &cfg).is_ok());
    }

    #[test]
    fn tracker_url_invalid() {
        let cfg = ssrf_config();
        assert!(matches!(
            validate_tracker_url("not a url", &cfg),
            Err(UrlGuardError::InvalidUrl(_))
        ));
    }

    #[test]
    fn tracker_url_idna_rejected() {
        let cfg = ssrf_config();
        // Use a punycode-encoded domain since url crate normalises.
        assert!(matches!(
            validate_tracker_url("http://xn--nxasmq6b.example.com/announce", &cfg),
            Err(UrlGuardError::IdnaDomain(_))
        ));
    }

    #[test]
    fn tracker_url_idna_allowed() {
        let cfg = permissive_config();
        assert!(validate_tracker_url("http://xn--nxasmq6b.example.com/announce", &cfg).is_ok());
    }

    // ── Web seed URL validation ──

    #[test]
    fn web_seed_url_valid_public() {
        let cfg = ssrf_config();
        assert!(validate_web_seed_url("http://cdn.example.com/files/", &cfg).is_ok());
        assert!(validate_web_seed_url("https://cdn.example.com/files/?token=abc", &cfg).is_ok());
    }

    #[test]
    fn web_seed_url_local_no_query() {
        let cfg = ssrf_config();
        assert!(validate_web_seed_url("http://192.168.1.100/files/", &cfg).is_ok());
        assert!(validate_web_seed_url("http://10.0.0.1/data/", &cfg).is_ok());
    }

    #[test]
    fn web_seed_url_local_with_query() {
        let cfg = ssrf_config();
        assert!(matches!(
            validate_web_seed_url("http://192.168.1.100/files/?secret=abc", &cfg),
            Err(UrlGuardError::LocalNetworkQueryString)
        ));
        assert!(matches!(
            validate_web_seed_url("http://localhost/files/?key=val", &cfg),
            Err(UrlGuardError::LocalNetworkQueryString)
        ));
    }

    #[test]
    fn web_seed_url_local_query_ssrf_disabled() {
        let mut cfg = ssrf_config();
        cfg.ssrf_mitigation = false;
        assert!(validate_web_seed_url("http://192.168.1.100/files/?secret=abc", &cfg).is_ok());
    }

    #[test]
    fn web_seed_url_idna_rejected() {
        let cfg = ssrf_config();
        assert!(matches!(
            validate_web_seed_url("http://xn--nxasmq6b.example.com/files/", &cfg),
            Err(UrlGuardError::IdnaDomain(_))
        ));
    }

    // ── Redirect validation ──

    #[test]
    fn redirect_public_to_public() {
        let cfg = ssrf_config();
        let orig = Url::parse("http://tracker.example.com/announce").unwrap();
        let redir = Url::parse("http://other.example.com/announce").unwrap();
        assert!(validate_redirect(&orig, &redir, &cfg).is_ok());
    }

    #[test]
    fn redirect_public_to_private_blocked() {
        let cfg = ssrf_config();
        let orig = Url::parse("http://tracker.example.com/announce").unwrap();
        let redir = Url::parse("http://192.168.1.1/announce").unwrap();
        assert!(matches!(
            validate_redirect(&orig, &redir, &cfg),
            Err(UrlGuardError::RedirectToPrivateIp(_))
        ));
    }

    #[test]
    fn redirect_public_to_localhost_blocked() {
        let cfg = ssrf_config();
        let orig = Url::parse("http://tracker.example.com/announce").unwrap();
        let redir = Url::parse("http://127.0.0.1/announce").unwrap();
        assert!(matches!(
            validate_redirect(&orig, &redir, &cfg),
            Err(UrlGuardError::RedirectToPrivateIp(_))
        ));

        let redir_v6 = Url::parse("http://[::1]/announce").unwrap();
        assert!(matches!(
            validate_redirect(&orig, &redir_v6, &cfg),
            Err(UrlGuardError::RedirectToPrivateIp(_))
        ));
    }

    #[test]
    fn redirect_public_to_localhost_domain_blocked() {
        let cfg = ssrf_config();
        let orig = Url::parse("http://tracker.example.com/announce").unwrap();
        let redir = Url::parse("http://localhost/announce").unwrap();
        assert!(matches!(
            validate_redirect(&orig, &redir, &cfg),
            Err(UrlGuardError::RedirectToPrivateIp(_))
        ));
    }

    #[test]
    fn redirect_private_to_private_allowed() {
        let cfg = ssrf_config();
        let orig = Url::parse("http://192.168.1.1/announce").unwrap();
        let redir = Url::parse("http://10.0.0.1/announce").unwrap();
        assert!(validate_redirect(&orig, &redir, &cfg).is_ok());
    }

    #[test]
    fn redirect_private_to_public_allowed() {
        let cfg = ssrf_config();
        let orig = Url::parse("http://192.168.1.1/announce").unwrap();
        let redir = Url::parse("http://tracker.example.com/announce").unwrap();
        assert!(validate_redirect(&orig, &redir, &cfg).is_ok());
    }

    #[test]
    fn redirect_ssrf_disabled() {
        let mut cfg = ssrf_config();
        cfg.ssrf_mitigation = false;
        let orig = Url::parse("http://tracker.example.com/announce").unwrap();
        let redir = Url::parse("http://192.168.1.1/announce").unwrap();
        assert!(validate_redirect(&orig, &redir, &cfg).is_ok());
    }

    // ── HTTP client builder ──

    #[test]
    fn build_client_default_config() {
        let cfg = ssrf_config();
        let client = build_http_client(&cfg, None, "Ferrite/0.58.0");
        // Just verify it builds without panicking.
        drop(client);
    }

    #[test]
    fn build_client_with_proxy() {
        let cfg = ssrf_config();
        let client = build_http_client(&cfg, Some("http://proxy.example.com:8080"), "Ferrite/0.58.0");
        drop(client);
    }

    #[test]
    fn build_client_invalid_proxy_fallback() {
        let cfg = ssrf_config();
        // Invalid proxy URL — should still build a client (proxy is silently skipped).
        let client = build_http_client(&cfg, Some("not a url"), "Ferrite/0.58.0");
        drop(client);
    }

    #[test]
    fn build_client_permissive_config() {
        let cfg = permissive_config();
        let client = build_http_client(&cfg, None, "Ferrite/0.58.0");
        drop(client);
    }

    #[test]
    fn build_redirect_policy_ssrf_enabled() {
        let cfg = ssrf_config();
        let _policy = build_redirect_policy(&cfg);
    }

    #[test]
    fn build_redirect_policy_ssrf_disabled() {
        let mut cfg = ssrf_config();
        cfg.ssrf_mitigation = false;
        let _policy = build_redirect_policy(&cfg);
    }

    // ── Integration / scenario tests ──

    #[test]
    fn scenario_malicious_torrent_ssrf_via_tracker() {
        let cfg = ssrf_config();
        let err = validate_tracker_url("http://127.0.0.1:9090/api/admin/delete-all", &cfg)
            .unwrap_err();
        assert!(matches!(err, UrlGuardError::LocalhostBadPath(_)));
        assert!(validate_tracker_url("http://127.0.0.1:9090/announce", &cfg).is_ok());
    }

    #[test]
    fn scenario_malicious_torrent_ssrf_via_web_seed() {
        let cfg = ssrf_config();
        let err = validate_web_seed_url("http://192.168.1.1/api?action=reboot", &cfg).unwrap_err();
        assert!(matches!(err, UrlGuardError::LocalNetworkQueryString));
    }

    #[test]
    fn scenario_redirect_ssrf() {
        let cfg = ssrf_config();
        let orig = Url::parse("http://evil-tracker.example.com/announce").unwrap();
        let redir = Url::parse("http://169.254.169.254/metadata/v1/").unwrap();
        let err = validate_redirect(&orig, &redir, &cfg).unwrap_err();
        assert!(matches!(err, UrlGuardError::RedirectToPrivateIp(_)));
    }

    #[test]
    fn scenario_legitimate_local_tracker() {
        let cfg = ssrf_config();
        assert!(validate_tracker_url("http://192.168.1.100:6969/announce", &cfg).is_ok());
        assert!(validate_tracker_url("http://[fe80::1]:6969/announce", &cfg).is_ok());
    }

    #[test]
    fn scenario_homograph_attack() {
        let cfg = ssrf_config();
        // Cyrillic 'а' (U+0430) looks identical to Latin 'a'.
        // The url crate punycode-encodes non-ASCII hostnames, so test with the
        // pre-encoded form. With allow_idna: false the xn-- label is rejected,
        // preventing homograph-based tracker substitution attacks.
        assert!(matches!(
            validate_tracker_url("http://xn--nxasmq6b.evil.com/announce", &cfg),
            Err(UrlGuardError::IdnaDomain(_))
        ));
    }

    #[test]
    fn scenario_all_protections_disabled() {
        let cfg = UrlSecurityConfig {
            ssrf_mitigation: false,
            allow_idna: true,
            validate_https_trackers: true,
        };
        assert!(validate_tracker_url("http://127.0.0.1:9090/admin", &cfg).is_ok());
        assert!(validate_web_seed_url("http://10.0.0.1/data?cmd=exec", &cfg).is_ok());
        let orig = Url::parse("http://tracker.example.com/announce").unwrap();
        let redir = Url::parse("http://127.0.0.1/admin").unwrap();
        assert!(validate_redirect(&orig, &redir, &cfg).is_ok());
    }
}
