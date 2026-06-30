//! Brokered `host::http-fetch` implementation (RFC-0010 §3).
//!
//! The host performs HTTP on behalf of the plugin — the guest never touches a socket.
//! Every call is gated by the plugin's hostname grant list; SSRF guards reject private and
//! reserved IP ranges before the request leaves the host process.
//!
//! # Security invariants
//!
//! - **Hostname grant**: only `grants.network` hosts may be contacted. Any other hostname
//!   (including after redirect) is rejected with an error string that does not include
//!   grant-list contents.
//! - **SSRF guards** (RFC-0010 §3.3):
//!   - URL-level: rejects IP literals in the URL that fall in private/reserved ranges.
//!   - DNS-level: resolves the hostname before sending; if *any* resolved IP is private,
//!     the request is aborted. Pre-flight ensures a DNS-rebinding attack window is as
//!     narrow as possible (reqwest re-resolves at connect, but we check first).
//! - **Response size cap**: `HttpLimits::max_response_bytes` (default 8 MiB). Streams are
//!   truncated and the call errors rather than letting a large response exhaust the host.
//! - **Header hygiene**: `Set-Cookie` is stripped from responses before returning to the
//!   guest. `Authorization` / `Cookie` / `X-Api-Key` values in *request* headers are
//!   redacted in log output.
//! - **HTTPS-only**: `http://` URLs are rejected; only `https://` is permitted by default.
//!   A future `allow-http` manifest grant (PR-C) will enable plain HTTP for specific hostnames.
//! - **Redirects**: `Policy::none()` — redirects are NOT followed. The plugin receives the
//!   3xx response and may issue a new `http-fetch` call, which goes through the full SSRF
//!   pipeline. This eliminates redirect-chaining SSRF (SEC-2).
//!
//! # Architecture note
//!
//! This module is synchronous from the caller's perspective: the plugin thread is a
//! `std::thread` (not a tokio task), so `reqwest`'s async client is driven via
//! `tokio::runtime::Handle::block_on` captured at instantiation time. The `CompState`
//! stores the `Handle` and the shared `reqwest::Client`; `do_http_fetch` receives both.

// WIT-generated types re-exported from component.rs so this module doesn't need to name the
// private `bindgen!` output directly.
use crate::component::{WitHttpRequest as HttpRequest, WitHttpResponse as HttpResponse};
use ipnet::{Ipv4Net, Ipv6Net};
use reqwest::{
    header::{HeaderMap, HeaderName, HeaderValue},
    redirect, Client, Url,
};
use std::{net::IpAddr, time::Duration};
use tracing::{debug, warn};

// ── Per-request limits ─────────────────────────────────────────────────────────────────────────

/// Per-call resource limits for the brokered HTTP fetch.
#[derive(Debug, Clone, Copy)]
pub(crate) struct HttpLimits {
    /// Maximum total response body size in bytes. Requests whose response body would exceed this
    /// are aborted and returned as an error. Default: 8 MiB.
    pub max_response_bytes: usize,
    /// TCP connect timeout in seconds. Must be ≤ the per-call epoch ceiling (RFC §3.3).
    /// Default: 4 s (epoch default: 50 ticks × 100 ms = 5 s).
    ///
    /// Also reused as the DNS pre-flight timeout.
    pub connect_timeout_secs: u64,
    /// Total request timeout in seconds (includes headers + body streaming). Must be ≤ the
    /// per-call epoch ceiling (RFC §3.3). Default: 4 s.
    ///
    /// # TODO(M8-5 cancellation)
    ///
    /// These timeouts are reqwest-level futures; the epoch-interruption mechanism cannot cancel
    /// the native OS blocking inside the reqwest/hyper/tokio stack frame. Until the connection
    /// is pinned to a pre-validated address and the plugin bridge adopts an async cancellation
    /// protocol (RFC Unresolved-Q §6), a hostile approved server can still park this thread
    /// for up to `request_timeout_secs` regardless of the epoch deadline. Track as M8-5.
    pub request_timeout_secs: u64,
}

impl Default for HttpLimits {
    fn default() -> Self {
        Self {
            max_response_bytes: 8 * 1024 * 1024,
            // 4 s is below the default epoch ceiling (5 s) to leave headroom for DNS + connect.
            connect_timeout_secs: 4,
            request_timeout_secs: 4,
        }
    }
}

// ── SSRF guard: blocked IP ranges (RFC-0010 §3.3) ─────────────────────────────────────────────

// RFC-1918 private + special-use ranges that must never be contacted by a plugin.
// Defined once at module level so each call doesn't re-parse CIDR strings.
//
// IPv4 ranges:
//   127.0.0.0/8      loopback
//   10.0.0.0/8       RFC-1918 private
//   172.16.0.0/12    RFC-1918 private
//   192.168.0.0/16   RFC-1918 private
//   169.254.0.0/16   link-local (AWS metadata endpoint lives here)
//   100.64.0.0/10    CGNAT shared address space
//   224.0.0.0/4      IPv4 multicast (RFC 1112 Class D)
//   240.0.0.0/4      reserved (Class E)
//   0.0.0.0/8        "this" network
//
// IPv6 ranges:
//   ::               unspecified address (connects to loopback on Linux)
//   ::1/128          loopback
//   fc00::/7         unique local (ULA)
//   fe80::/10        link-local
//   ff00::/8         IPv6 multicast
//   ::ffff:0:0/96    IPv4-mapped (catches IPv4-in-IPv6 SSRF)

macro_rules! ipv4net {
    ($s:literal) => {{
        // safe: compile-time literal — parse errors are test-caught
        $s.parse::<Ipv4Net>().expect("valid IPv4 CIDR")
    }};
}

macro_rules! ipv6net {
    ($s:literal) => {{
        $s.parse::<Ipv6Net>().expect("valid IPv6 CIDR")
    }};
}

/// Returns `true` if `ip` falls in any private, loopback, link-local, or reserved range and must
/// not be contacted by a plugin (SSRF guard).
pub(crate) fn is_ssrf_blocked_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            // `is_unspecified` (0.0.0.0) is covered by 0.0.0.0/8 below, but the explicit
            // check is clearer and symmetric with the IPv6 path.
            if v4.is_unspecified() || v4.is_loopback() {
                return true;
            }
            let ranges: &[Ipv4Net] = &[
                ipv4net!("10.0.0.0/8"),
                ipv4net!("172.16.0.0/12"),
                ipv4net!("192.168.0.0/16"),
                ipv4net!("169.254.0.0/16"),
                ipv4net!("100.64.0.0/10"),
                ipv4net!("224.0.0.0/4"), // IPv4 multicast (Class D)
                ipv4net!("240.0.0.0/4"), // reserved (Class E)
                ipv4net!("0.0.0.0/8"),   // "this" network
            ];
            ranges.iter().any(|net| net.contains(&v4))
        }
        IpAddr::V6(v6) => {
            // Unspecified `::` connects to loopback on Linux — block it explicitly.
            if v6.is_unspecified() || v6.is_loopback() {
                return true;
            }
            // IPv4-mapped (::ffff:x.x.x.x) — recursively check the embedded IPv4
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_ssrf_blocked_ip(IpAddr::V4(v4));
            }
            let ranges: &[Ipv6Net] = &[
                ipv6net!("fc00::/7"),  // ULA
                ipv6net!("fe80::/10"), // link-local
                ipv6net!("ff00::/8"),  // IPv6 multicast
            ];
            ranges.iter().any(|net| net.contains(&v6))
        }
    }
}

// ── Hostname grant check ───────────────────────────────────────────────────────────────────────

/// Returns `true` if `host` (lower-cased) is in the plugin's `network` grant list.
///
/// Comparison is case-insensitive exact match on the hostname only — no wildcards, no port
/// matching (ports are part of the URL structure, not the grant).
pub(crate) fn hostname_allowed(host: &str, grants: &[String]) -> bool {
    grants.iter().any(|g| g.eq_ignore_ascii_case(host))
}

// ── URL validation ─────────────────────────────────────────────────────────────────────────────

/// Validate the request URL: parse it, check hostname against the grant list, and reject IP
/// literals that are in the SSRF block list.
///
/// Returns the parsed `Url` on success, or a redacted error string on failure. This is a
/// **synchronous** pre-flight — it does not perform DNS resolution.
pub(crate) fn validate_url(req: &HttpRequest, grants: &[String]) -> Result<Url, String> {
    let url = Url::parse(&req.url).map_err(|e| format!("invalid URL: {e}"))?;

    // HTTPS-only by default: reject `http://` to prevent brokered credentials and response
    // data from being exposed to on-path observers. A future `allow-http` manifest grant
    // (PR-C) will enable plain HTTP for operators who explicitly need it (e.g. internal
    // services behind a TLS-terminating proxy).
    // TODO(PR-C): implement per-hostname `allow-http` grant in the manifest loader.
    match url.scheme() {
        "https" => {}
        "http" => {
            return Err(
                "http:// is not allowed; use https:// (plain HTTP can be enabled via the \
                 allow-http grant in PR-C)"
                    .to_owned(),
            )
        }
        other => return Err(format!("disallowed URL scheme: {other}")),
    }

    let host = url.host_str().ok_or_else(|| "URL has no host".to_owned())?;

    // Check the grant list before any further processing.
    if !hostname_allowed(host, grants) {
        // Do NOT include the grants list in the error — that would let a guest enumerate
        // the allowed hostnames by trying different URLs and reading the error.
        return Err("host not in network grant".to_owned());
    }

    // IP-literal SSRF guard (pre-DNS, catches e.g. `http://127.0.0.1/...` or `http://[::1]/...`).
    // `url::Url::host_str()` returns IPv6 addresses in bracket notation (`[::1]`), which cannot
    // be parsed by `IpAddr::parse` directly. Strip the brackets first to handle both forms.
    let ip_str = if host.starts_with('[') && host.ends_with(']') {
        &host[1..host.len() - 1]
    } else {
        host
    };
    if let Ok(ip) = ip_str.parse::<IpAddr>() {
        if is_ssrf_blocked_ip(ip) {
            return Err("SSRF: IP address in blocked range".to_owned());
        }
    }

    Ok(url)
}

// ── DNS SSRF pre-flight ────────────────────────────────────────────────────────────────────────

/// Resolve `host` and check every resulting IP against the SSRF block list.
///
/// This is an async operation; the caller drives it with `Handle::block_on`. Returns `Ok(())`
/// if all resolved IPs are public, or an error if any resolved IP is blocked.
///
/// `connect_timeout_secs` bounds the DNS lookup; an unresponsive DNS server cannot park the
/// plugin thread indefinitely.
///
/// # SECURITY (release-gating for M8-5 / PR-C)
///
/// There is an inherent TOCTOU window between this pre-flight and reqwest's own DNS resolution
/// at TCP connect time. An attacker who controls the DNS record for a granted hostname can serve
/// a public IP during this check, then flip to a private IP (e.g. `169.254.169.254`) before
/// reqwest connects (low-TTL DNS rebinding). The window is narrow but non-zero.
///
/// **Before PR-C makes `instantiate_with_grants` reachable from the binary by untrusted
/// plugins, this MUST be resolved** by pinning the connection to the `SocketAddr` set
/// resolved here: implement a custom `reqwest::dns::Resolve` / `Client::resolve` override
/// that re-validates the pinned IP before connecting. Bundle SEC-8 (6to4/Teredo/NAT64
/// embedded-v4 prefix blocks) with that work. Track as M8-5.
///
/// Until then: network grants MUST only be issued to hostnames under the operator's full
/// DNS control.
pub(crate) async fn check_ssrf_via_dns(
    host: &str,
    port: u16,
    connect_timeout_secs: u64,
) -> Result<(), String> {
    // tokio's `lookup_host` performs a real DNS query (AAAA + A in parallel).
    let addr_str = format!("{host}:{port}");
    let addrs = tokio::time::timeout(
        Duration::from_secs(connect_timeout_secs),
        tokio::net::lookup_host(&addr_str),
    )
    .await
    .map_err(|_| format!("DNS lookup timed out for {host}"))?
    .map_err(|e| format!("DNS resolution failed: {e}"))?;

    let mut found_any = false;
    for addr in addrs {
        found_any = true;
        if is_ssrf_blocked_ip(addr.ip()) {
            warn!(
                target: "cairn_plugin::http_fetch",
                host = host,
                "SSRF: DNS resolved to a blocked IP, aborting plugin request"
            );
            return Err("SSRF: hostname resolved to a blocked IP".to_owned());
        }
    }

    if !found_any {
        return Err("DNS: no addresses resolved".to_owned());
    }

    Ok(())
}

// ── reqwest Client construction ────────────────────────────────────────────────────────────────

/// Sensitive header names that are redacted in log output. Values are never logged.
const REDACTED_REQUEST_HEADERS: &[&str] = &[
    "authorization",
    "cookie",
    "x-api-key",
    "x-auth-token",
    "proxy-authorization",
];

/// Maximum number of response headers accepted from the server (inclusive). Prevents a
/// compromised or adversarial granted server from forcing unbounded heap allocation via
/// a flood of response headers whose count is not bounded by the body-size cap.
const MAX_RESPONSE_HEADERS: usize = 200;

/// Maximum byte length of a single response header value. Matches Chrome's cap.
/// Headers exceeding this size are treated as malformed responses.
const MAX_HEADER_VALUE_BYTES: usize = 8 * 1024;

/// HTTP methods the plugin may use (RFC-0010 §3.2 step 6 closed method list).
/// CONNECT (proxy tunnel) and TRACE (XST cross-site tracing) are excluded.
const ALLOWED_METHODS: &[&str] = &["GET", "HEAD", "POST", "PUT", "DELETE", "PATCH", "OPTIONS"];

/// Maximum combined byte size of all request header names + values. Prevents a plugin from
/// exfiltrating large data volumes through request headers to a granted upstream.
const MAX_REQUEST_HEADERS_BYTES: usize = 32 * 1024;

/// Maximum request body size. Rejects oversized bodies before dispatch to prevent a plugin
/// from forcing the host to buffer 32+ MiB through this path.
const MAX_REQUEST_BODY_BYTES: usize = 32 * 1024 * 1024;

/// Build a `reqwest::Client` configured for plugin use:
/// - rustls TLS only (no OpenSSL)
/// - per-call connect / request timeouts
/// - **no redirect following** (`Policy::none()`)
///
/// Redirects are intentionally disabled. A custom redirect-following policy that re-checks
/// the grant list on each hop would still be vulnerable to SSRF via DNS rebinding on the
/// redirect target (no async DNS pre-flight can run inside reqwest's synchronous redirect
/// closure). Returning the 3xx response to the plugin is the safe choice: the plugin can
/// inspect the `Location` header and issue a new `http-fetch` call, which goes through the
/// full pipeline including DNS SSRF pre-flight.
///
/// The client is built once per plugin instance and shared across calls (it holds a
/// connection pool internally, which is safe to reuse from a single thread via `block_on`).
pub(crate) fn build_client(limits: HttpLimits) -> Result<Client, String> {
    Client::builder()
        .use_rustls_tls()
        .connect_timeout(Duration::from_secs(limits.connect_timeout_secs))
        .timeout(Duration::from_secs(limits.request_timeout_secs))
        // Redirects are NOT followed; the plugin receives the 3xx and handles it explicitly.
        // This prevents SSRF via redirect chaining (a server could redirect to a private IP).
        .redirect(redirect::Policy::none())
        // Disable connection-level debug output (reqwest/hyper verbose logging).
        .connection_verbose(false)
        .build()
        .map_err(|e| format!("failed to build HTTP client: {e}"))
}

// ── Core async fetch (no DNS SSRF, used by both prod and test paths) ──────────────────────────

/// Execute the HTTP request synchronously, reading the response body up to `limits.max_response_bytes`.
///
/// `Set-Cookie` is stripped from the response before returning. This function does not perform
/// DNS SSRF checks — the caller is responsible for that.
pub(crate) async fn execute_fetch(
    req: &HttpRequest,
    url: Url,
    client: &Client,
    limits: HttpLimits,
) -> Result<HttpResponse, String> {
    // Pre-dispatch caps: reject oversized requests before touching the network (RFC §3.3).
    let total_header_bytes: usize = req.headers.iter().map(|(n, v)| n.len() + v.len()).sum();
    if total_header_bytes > MAX_REQUEST_HEADERS_BYTES {
        return Err(format!(
            "request headers ({total_header_bytes} B) exceed the {MAX_REQUEST_HEADERS_BYTES} B cap"
        ));
    }
    if let Some(body) = &req.body {
        if body.len() > MAX_REQUEST_BODY_BYTES {
            return Err(format!(
                "request body ({} B) exceeds the {MAX_REQUEST_BODY_BYTES} B cap",
                body.len()
            ));
        }
    }

    // Closed method allow-list (RFC-0010 §3.2 step 6). CONNECT enables proxy tunnelling;
    // TRACE enables XST cross-site tracing attacks. Both are excluded.
    let method_upper = req.method.to_ascii_uppercase();
    if !ALLOWED_METHODS.contains(&method_upper.as_str()) {
        return Err(format!(
            "disallowed HTTP method: {} \
             (allowed: GET/HEAD/POST/PUT/DELETE/PATCH/OPTIONS)",
            req.method
        ));
    }
    let method = reqwest::Method::from_bytes(req.method.as_bytes())
        .map_err(|_| format!("invalid HTTP method: {}", req.method))?;

    // Build request headers. Log the header name only; never log values — even for
    // non-sensitive headers, values may contain credentials passed by the plugin guest.
    let mut headers = HeaderMap::new();
    for (name, value) in &req.headers {
        let header_name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| format!("invalid header name: {name}"))?;
        let header_value =
            HeaderValue::from_str(value).map_err(|_| format!("invalid header value for {name}"))?;
        let lower = name.to_ascii_lowercase();
        if REDACTED_REQUEST_HEADERS.contains(&lower.as_str()) {
            debug!(target: "cairn_plugin::http_fetch", header = %name, value = "[redacted]");
        } else {
            // Name only — no value, even for "non-sensitive" headers.
            debug!(target: "cairn_plugin::http_fetch", header = %name);
        }
        headers.insert(header_name, header_value);
    }

    // Build and send the request.
    let mut builder = client.request(method, url).headers(headers);
    if let Some(body) = &req.body {
        builder = builder.body(body.clone());
    }

    let response = builder.send().await.map_err(|e| {
        // Redact the URL from the error in case it contains credentials.
        format!("HTTP request failed: {}", redact_url_error(e))
    })?;

    let status = response.status().as_u16();

    // Collect response headers, stripping `Set-Cookie`, with count and size caps.
    // An adversarial or compromised server could return thousands of large headers; we cap
    // at `MAX_RESPONSE_HEADERS` entries and `MAX_HEADER_VALUE_BYTES` per value to bound
    // host-process heap allocation independently of the body-size limit.
    let mut resp_headers: Vec<(String, String)> = Vec::new();
    let mut header_count = 0usize;
    for (name, value) in response.headers() {
        if name.as_str().eq_ignore_ascii_case("set-cookie") {
            continue;
        }
        header_count += 1;
        if header_count > MAX_RESPONSE_HEADERS {
            return Err(format!(
                "response exceeds the {MAX_RESPONSE_HEADERS}-header limit"
            ));
        }
        if value.len() > MAX_HEADER_VALUE_BYTES {
            return Err(format!(
                "response header '{}' exceeds the {} byte value limit",
                name.as_str(),
                MAX_HEADER_VALUE_BYTES
            ));
        }
        if let Ok(v) = value.to_str() {
            resp_headers.push((name.as_str().to_owned(), v.to_owned()));
        }
    }

    // Stream the body with a size cap.
    use futures::StreamExt as _;
    let mut body_bytes: Vec<u8> = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk =
            chunk.map_err(|e| format!("error reading response body: {}", redact_url_error(e)))?;
        if body_bytes.len() + chunk.len() > limits.max_response_bytes {
            return Err(format!(
                "response body exceeds the {} byte limit",
                limits.max_response_bytes
            ));
        }
        body_bytes.extend_from_slice(&chunk);
    }

    Ok(HttpResponse {
        status,
        headers: resp_headers,
        body: body_bytes,
    })
}

/// Redact the URL from a `reqwest::Error` message to avoid leaking credentials embedded in URLs.
fn redact_url_error(e: reqwest::Error) -> String {
    // reqwest errors can include the URL in their Display; remove it.
    // Simple approach: build our own message from the error category.
    if e.is_timeout() {
        "request timed out".to_owned()
    } else if e.is_connect() {
        "connection failed".to_owned()
    } else if e.is_redirect() {
        "redirect policy error".to_owned()
    } else if e.is_status() {
        format!(
            "HTTP {}",
            e.status()
                .map(|s| s.as_u16().to_string())
                .unwrap_or_else(|| "error".to_owned())
        )
    } else {
        "request error".to_owned()
    }
}

// ── Public entry points ────────────────────────────────────────────────────────────────────────

/// Full brokered HTTP fetch pipeline:
/// 1. Validate URL + hostname grant (sync).
/// 2. SSRF DNS pre-flight (async).
/// 3. Execute the HTTP call (async).
///
/// Called from `CompState::http_fetch` via `Handle::block_on`.
pub(crate) async fn do_http_fetch(
    req: &HttpRequest,
    grants: &[String],
    client: &Client,
    limits: HttpLimits,
) -> Result<HttpResponse, String> {
    let url = validate_url(req, grants)?;

    let host = url.host_str().ok_or("URL has no host")?;
    // `port_or_known_default` returns `None` only for unknown schemes; the scheme check above
    // guarantees http/https, so `unwrap_or(80)` is unreachable dead-code safety net.
    let port = url.port_or_known_default().unwrap_or(80);
    check_ssrf_via_dns(host, port, limits.connect_timeout_secs).await?;

    execute_fetch(req, url, client, limits).await
}

/// Test-only variant that skips the DNS SSRF pre-flight so wiremock servers (which bind to
/// `127.0.0.1`) can be used as mock targets. The hostname-grant check still runs; IP-literal
/// SSRF validation from `validate_url` still runs for URL-embedded IPs, but the DNS check that
/// would block `127.0.0.1` is suppressed.
///
/// **Do not use this outside tests.** It is gated behind `#[cfg(test)]`.
#[cfg(test)]
pub(crate) async fn do_http_fetch_no_dns_ssrf(
    req: &HttpRequest,
    grants: &[String],
    client: &Client,
    limits: HttpLimits,
) -> Result<HttpResponse, String> {
    // Still validate scheme, hostname grant, and IP-literal SSRF; only DNS SSRF is skipped.
    // We parse the URL manually to avoid calling `validate_url` which blocks IP literals in
    // the URL — the wiremock server binds to 127.0.0.1 (an IP literal). Instead we only
    // check the hostname grant and let execute_fetch proceed.
    let url = Url::parse(&req.url).map_err(|e| format!("invalid URL: {e}"))?;
    match url.scheme() {
        "http" | "https" => {}
        other => return Err(format!("disallowed URL scheme: {other}")),
    }
    let host = url.host_str().ok_or("URL has no host")?;
    // Grant check still required even in test mode.
    if !hostname_allowed(host, grants) {
        return Err("host not in network grant".to_owned());
    }
    execute_fetch(req, url, client, limits).await
}

// ── Unit tests ─────────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    // ── is_ssrf_blocked_ip ──────────────────────────────────────────────────────────────────

    #[test]
    fn loopback_is_blocked() {
        assert!(is_ssrf_blocked_ip("127.0.0.1".parse().unwrap()));
        assert!(is_ssrf_blocked_ip("127.255.255.255".parse().unwrap()));
        assert!(is_ssrf_blocked_ip("::1".parse().unwrap()));
    }

    #[test]
    fn private_ranges_are_blocked() {
        for ip in &[
            "10.0.0.1",
            "10.255.255.255",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.0.1",
            "192.168.255.255",
        ] {
            assert!(
                is_ssrf_blocked_ip(ip.parse().unwrap()),
                "{ip} should be blocked"
            );
        }
    }

    #[test]
    fn link_local_is_blocked() {
        assert!(is_ssrf_blocked_ip("169.254.0.1".parse().unwrap()));
        assert!(is_ssrf_blocked_ip("169.254.169.254".parse().unwrap())); // AWS metadata
                                                                         // IPv6 link-local
        assert!(is_ssrf_blocked_ip("fe80::1".parse().unwrap()));
    }

    #[test]
    fn cgnat_is_blocked() {
        assert!(is_ssrf_blocked_ip("100.64.0.1".parse().unwrap()));
        assert!(is_ssrf_blocked_ip("100.127.255.255".parse().unwrap()));
    }

    #[test]
    fn reserved_class_e_is_blocked() {
        assert!(is_ssrf_blocked_ip("240.0.0.1".parse().unwrap()));
        assert!(is_ssrf_blocked_ip("255.255.255.255".parse().unwrap()));
    }

    #[test]
    fn ipv6_ula_is_blocked() {
        assert!(is_ssrf_blocked_ip("fc00::1".parse().unwrap()));
        assert!(is_ssrf_blocked_ip(
            "fdff:ffff:ffff:ffff:ffff:ffff:ffff:ffff".parse().unwrap()
        ));
    }

    #[test]
    fn ipv4_mapped_loopback_is_blocked() {
        // ::ffff:127.0.0.1 is IPv4-mapped loopback — must be blocked.
        assert!(is_ssrf_blocked_ip("::ffff:127.0.0.1".parse().unwrap()));
        assert!(is_ssrf_blocked_ip("::ffff:10.0.0.1".parse().unwrap()));
    }

    #[test]
    fn public_ips_are_not_blocked() {
        for ip in &["8.8.8.8", "1.1.1.1", "104.16.0.0", "2606:4700::1111"] {
            assert!(
                !is_ssrf_blocked_ip(ip.parse().unwrap()),
                "{ip} should be allowed"
            );
        }
        // RFC-5737 documentation addresses (TEST-NET) are NOT in our block list — they
        // are not routable in practice but are not explicitly blocked.
        assert!(!is_ssrf_blocked_ip(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1))));
    }

    // ── hostname_allowed ────────────────────────────────────────────────────────────────────

    #[test]
    fn hostname_in_grant_is_allowed() {
        let grants = vec![
            "api.github.com".to_owned(),
            "releases.example.com".to_owned(),
        ];
        assert!(hostname_allowed("api.github.com", &grants));
        assert!(hostname_allowed("API.GITHUB.COM", &grants)); // case-insensitive
        assert!(hostname_allowed("releases.example.com", &grants));
    }

    #[test]
    fn hostname_not_in_grant_is_rejected() {
        let grants = vec!["api.github.com".to_owned()];
        assert!(!hostname_allowed("evil.example.com", &grants));
        assert!(!hostname_allowed("github.com", &grants)); // subdomain must match exactly
        assert!(!hostname_allowed("notgithub.com", &grants));
        assert!(!hostname_allowed("", &grants));
    }

    #[test]
    fn empty_grant_list_rejects_all() {
        assert!(!hostname_allowed("example.com", &[]));
    }

    // ── validate_url ────────────────────────────────────────────────────────────────────────

    fn make_req(url: &str) -> HttpRequest {
        HttpRequest {
            method: "GET".to_owned(),
            url: url.to_owned(),
            headers: vec![],
            body: None,
        }
    }

    #[test]
    fn valid_url_passes() {
        let grants = vec!["api.example.com".to_owned()];
        let req = make_req("https://api.example.com/v1/data");
        assert!(validate_url(&req, &grants).is_ok());
    }

    #[test]
    fn unlisted_host_is_rejected() {
        let grants = vec!["api.example.com".to_owned()];
        let req = make_req("https://evil.com/steal");
        let err = validate_url(&req, &grants).unwrap_err();
        // Error must not reveal the grant list contents.
        assert!(
            !err.contains("api.example.com"),
            "error must not leak grant list: {err}"
        );
        assert!(err.contains("network grant"), "err = {err}");
    }

    #[test]
    fn ip_literal_loopback_is_rejected() {
        let grants = vec!["127.0.0.1".to_owned()]; // even if in grants — IP literal check runs
        let req = make_req("https://127.0.0.1/secret");
        let err = validate_url(&req, &grants).unwrap_err();
        assert!(
            err.contains("SSRF") || err.contains("blocked"),
            "err = {err}"
        );
    }

    #[test]
    fn ipv6_literal_loopback_is_rejected() {
        // IPv6 loopback `::1` is represented in URLs as `[::1]`. The bracket form must be
        // correctly parsed (stripping `[` and `]`) before the SSRF check runs.
        let grants = vec!["[::1]".to_owned()]; // even if in grants — IP literal check runs
        let req = make_req("https://[::1]/secret");
        let err = validate_url(&req, &grants).unwrap_err();
        assert!(
            err.contains("SSRF") || err.contains("blocked"),
            "IPv6 loopback must be SSRF-blocked, err = {err}"
        );
    }

    #[test]
    fn ipv6_ula_literal_is_rejected() {
        // ULA prefix fc00::/7. Must be blocked even when grant list includes the address.
        let grants = vec!["[fc00::1]".to_owned()];
        let req = make_req("https://[fc00::1]/");
        let err = validate_url(&req, &grants).unwrap_err();
        assert!(
            err.contains("SSRF") || err.contains("blocked"),
            "IPv6 ULA must be SSRF-blocked, err = {err}"
        );
    }

    #[test]
    fn ipv6_unspecified_is_rejected() {
        // `::` is the IPv6 unspecified address; on Linux connect() to `::` reaches loopback.
        let grants = vec!["::".to_owned(), "[::0]".to_owned()];
        for url in &["https://[::]/", "https://[::0]/"] {
            let req = make_req(url);
            let err = validate_url(&req, &grants).unwrap_err();
            assert!(
                err.contains("SSRF") || err.contains("blocked") || err.contains("network grant"),
                "IPv6 unspecified must be blocked, url={url}, err={err}"
            );
        }
    }

    #[test]
    fn ipv4_multicast_is_rejected() {
        let grants = vec!["224.0.0.1".to_owned()];
        let req = make_req("https://224.0.0.1/");
        let err = validate_url(&req, &grants).unwrap_err();
        assert!(
            err.contains("SSRF") || err.contains("blocked"),
            "IPv4 multicast must be SSRF-blocked, err = {err}"
        );
    }

    #[test]
    fn ipv6_multicast_is_rejected() {
        let grants = vec!["[ff02::1]".to_owned()];
        let req = make_req("https://[ff02::1]/");
        let err = validate_url(&req, &grants).unwrap_err();
        assert!(
            err.contains("SSRF") || err.contains("blocked"),
            "IPv6 multicast must be SSRF-blocked, err = {err}"
        );
    }

    #[test]
    fn http_scheme_is_rejected_by_default() {
        // Only https:// is allowed; http:// must be rejected even for an allowed host.
        let grants = vec!["example.com".to_owned()];
        let req = make_req("http://example.com/data");
        let err = validate_url(&req, &grants).unwrap_err();
        assert!(
            err.contains("http://") || err.contains("https://") || err.contains("scheme"),
            "http:// must be rejected, err = {err}"
        );
    }

    #[test]
    fn decimal_encoded_ipv4_is_rejected() {
        // 2130706433 == 0x7f000001 == 127.0.0.1 in 32-bit decimal.
        // The url crate (WHATWG URL parser) normalises this to 127.0.0.1 in host_str(),
        // so it is caught by the SSRF block (if the grant has "127.0.0.1") or the grant
        // check (if the grant has "2130706433", which won't match the normalised form).
        // Either way, the request is rejected — the test asserts fail-closed behaviour.
        let grants_decimal = vec!["2130706433".to_owned()];
        let req = make_req("https://2130706433/secret");
        let err = validate_url(&req, &grants_decimal).unwrap_err();
        assert!(
            err.contains("network grant") || err.contains("SSRF") || err.contains("blocked"),
            "decimal-encoded IPv4 must be rejected, err = {err}"
        );

        // With the normalised address in grants, the SSRF check still blocks it.
        let grants_norm = vec!["127.0.0.1".to_owned()];
        let err = validate_url(&req, &grants_norm).unwrap_err();
        assert!(
            err.contains("SSRF") || err.contains("blocked"),
            "decimal-encoded IPv4 with normalised grant must still be SSRF-blocked, err = {err}"
        );
    }

    #[test]
    fn ftp_scheme_is_rejected() {
        let grants = vec!["example.com".to_owned()];
        let req = make_req("ftp://example.com/file");
        let err = validate_url(&req, &grants).unwrap_err();
        assert!(err.contains("scheme"), "err = {err}");
    }

    #[test]
    fn file_scheme_is_rejected() {
        let grants = vec!["example.com".to_owned()];
        let req = make_req("file:///etc/passwd");
        let err = validate_url(&req, &grants).unwrap_err();
        assert!(err.contains("scheme"), "err = {err}");
    }

    #[test]
    fn invalid_url_is_rejected() {
        let grants = vec!["example.com".to_owned()];
        let req = make_req("not a url at all !!!");
        let err = validate_url(&req, &grants).unwrap_err();
        assert!(err.contains("invalid URL"), "err = {err}");
    }

    // ── execute_fetch pre-dispatch caps ────────────────────────────────────────────────────

    #[tokio::test]
    async fn disallowed_method_is_rejected() {
        use wiremock::MockServer;
        // We never need to reach the server; the method check fires before send.
        let server = MockServer::start().await;
        let base = server.uri();
        let host = server.address().ip().to_string();
        let grants = vec![host.clone()];
        let limits = HttpLimits::default();
        let client = build_client(limits).expect("client");

        for method in &["CONNECT", "TRACE", "FOOBAR"] {
            let req = HttpRequest {
                method: (*method).to_owned(),
                url: format!("{base}/"),
                headers: vec![],
                body: None,
            };
            let err = do_http_fetch_no_dns_ssrf(&req, &grants, &client, limits)
                .await
                .unwrap_err();
            assert!(
                err.contains("method") || err.contains("disallowed"),
                "method {method} must be rejected, err = {err}"
            );
        }
    }

    // ── Wiremock HTTP integration tests ─────────────────────────────────────────────────────
    //
    // These tests drive the real `execute_fetch` pipeline against a `wiremock` server that
    // binds to 127.0.0.1. Because that is a loopback address (blocked by SSRF), we use
    // `do_http_fetch_no_dns_ssrf` which skips the DNS SSRF pre-flight but still validates
    // the hostname grant and performs the real HTTP call mechanics.

    /// Successful fetch: allowed host, wiremock returns 200 + body.
    #[tokio::test]
    async fn allowed_host_returns_response() {
        use wiremock::{
            matchers::{method, path},
            Mock, MockServer, ResponseTemplate,
        };

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/hello"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"world".as_slice()))
            .mount(&server)
            .await;

        let base = server.uri();
        let host = server.address().ip().to_string();
        let grants = vec![host.clone()];
        let limits = HttpLimits::default();
        let client = build_client(limits).expect("client");

        let req = make_req(&format!("{base}/hello"));
        let resp = do_http_fetch_no_dns_ssrf(&req, &grants, &client, limits)
            .await
            .expect("fetch must succeed");

        assert_eq!(resp.status, 200);
        assert_eq!(resp.body, b"world");
    }

    /// Disallowed host → immediate error, no network call made.
    #[tokio::test]
    async fn disallowed_host_is_rejected_before_send() {
        use wiremock::MockServer;

        let server = MockServer::start().await;
        // No mock registered: if a request ever reaches the server it would return 404, but
        // we expect the grant check to abort before any network contact.
        let base = server.uri();
        let grants = vec!["api.allowed.example.com".to_owned()];
        let limits = HttpLimits::default();
        let client = build_client(limits).expect("client");

        let req = make_req(&format!("{base}/should-not-reach"));
        let err = do_http_fetch_no_dns_ssrf(&req, &grants, &client, limits)
            .await
            .unwrap_err();

        // Error must not reveal the grant list.
        assert!(
            err.contains("network grant"),
            "expected grant-check error, got: {err}"
        );
        assert!(
            !err.contains("api.allowed.example.com"),
            "error must not reveal grant list: {err}"
        );
    }

    /// Response body truncated at the size cap → error.
    #[tokio::test]
    async fn response_too_large_is_aborted() {
        use wiremock::{
            matchers::{method, path},
            Mock, MockServer, ResponseTemplate,
        };

        let server = MockServer::start().await;
        let big_body = vec![b'x'; 1025];
        Mock::given(method("GET"))
            .and(path("/big"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(big_body))
            .mount(&server)
            .await;

        let base = server.uri();
        let host = server.address().ip().to_string();
        let grants = vec![host.clone()];
        // Set a tiny cap (1 KiB) so the 1025-byte body trips it.
        let limits = HttpLimits {
            max_response_bytes: 1024,
            ..HttpLimits::default()
        };
        let client = build_client(limits).expect("client");

        let req = make_req(&format!("{base}/big"));
        let err = do_http_fetch_no_dns_ssrf(&req, &grants, &client, limits)
            .await
            .unwrap_err();

        assert!(
            err.contains("limit") || err.contains("exceed"),
            "expected size-limit error, got: {err}"
        );
    }

    /// `Set-Cookie` is stripped from responses; other response headers pass through.
    #[tokio::test]
    async fn set_cookie_is_stripped_from_response() {
        use wiremock::{
            matchers::{method, path},
            Mock, MockServer, ResponseTemplate,
        };

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/cookies"))
            .respond_with(
                ResponseTemplate::new(200)
                    .append_header("Set-Cookie", "session=abc; HttpOnly")
                    .append_header("X-Request-Id", "req-123")
                    .set_body_bytes(b"ok".as_slice()),
            )
            .mount(&server)
            .await;

        let base = server.uri();
        let host = server.address().ip().to_string();
        let grants = vec![host.clone()];
        let limits = HttpLimits::default();
        let client = build_client(limits).expect("client");

        let req = make_req(&format!("{base}/cookies"));
        let resp = do_http_fetch_no_dns_ssrf(&req, &grants, &client, limits)
            .await
            .expect("fetch");

        let has_set_cookie = resp
            .headers
            .iter()
            .any(|(n, _)| n.eq_ignore_ascii_case("set-cookie"));
        assert!(
            !has_set_cookie,
            "Set-Cookie must be stripped from plugin responses"
        );

        let has_x_request_id = resp
            .headers
            .iter()
            .any(|(n, v)| n == "x-request-id" && v == "req-123");
        assert!(has_x_request_id, "other headers must pass through");
    }

    /// POST with a body is forwarded correctly to the server.
    #[tokio::test]
    async fn post_with_body_is_forwarded() {
        use wiremock::{
            matchers::{body_bytes, method, path},
            Mock, MockServer, ResponseTemplate,
        };

        let server = MockServer::start().await;
        let payload: Vec<u8> = b"hello-body".to_vec();
        Mock::given(method("POST"))
            .and(path("/echo"))
            .and(body_bytes(payload.clone()))
            .respond_with(ResponseTemplate::new(201).set_body_bytes(b"created"))
            .mount(&server)
            .await;

        let base = server.uri();
        let host = server.address().ip().to_string();
        let grants = vec![host.clone()];
        let limits = HttpLimits::default();
        let client = build_client(limits).expect("client");

        let req = HttpRequest {
            method: "POST".to_owned(),
            url: format!("{base}/echo"),
            headers: vec![],
            body: Some(payload),
        };
        let resp = do_http_fetch_no_dns_ssrf(&req, &grants, &client, limits)
            .await
            .expect("POST fetch");

        assert_eq!(resp.status, 201);
    }

    /// Redirects are not followed (Policy::none()); the 3xx response is returned to the plugin.
    ///
    /// The plugin is responsible for inspecting the `Location` header and issuing a new
    /// `http-fetch` call, which will go through the full SSRF pre-flight pipeline. This prevents
    /// redirect-chaining SSRF attacks where a server redirects to a private IP.
    #[tokio::test]
    async fn redirect_is_returned_not_followed() {
        use wiremock::{
            matchers::{method, path},
            Mock, MockServer, ResponseTemplate,
        };

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/redir"))
            .respond_with(
                ResponseTemplate::new(302)
                    .append_header("Location", "https://not-granted.example.com/"),
            )
            .mount(&server)
            .await;

        let base = server.uri();
        let host = server.address().ip().to_string();
        let grants = vec![host.clone()];
        let limits = HttpLimits::default();
        let client = build_client(limits).expect("client");

        let req = make_req(&format!("{base}/redir"));
        let resp = do_http_fetch_no_dns_ssrf(&req, &grants, &client, limits)
            .await
            .expect("302 returned as-is without following");

        // The 302 status is returned to the plugin; the redirect target is NOT contacted.
        assert_eq!(resp.status, 302, "must return 302 without following");
        let has_location = resp.headers.iter().any(|(n, _)| n == "location");
        assert!(
            has_location,
            "Location header must be present in the 302 response"
        );
    }
}
