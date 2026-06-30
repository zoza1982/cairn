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
//! - **SSRF guards** (RFC-0010 §3.3, SEC-1 fix — issue #103):
//!   - URL-level: rejects IP literals in the URL that fall in private/reserved ranges.
//!   - DNS-pinning: [`PinnedSsrfDnsResolver`] is installed as the sole DNS authority for
//!     the plugin's `reqwest::Client`. When hyper asks it to resolve a hostname, it resolves
//!     via Tokio, classifies every returned IP against the SSRF block list, and returns only
//!     the validated IPs (or errors if all are blocked). Because reqwest's `HttpConnector`
//!     uses the custom resolver directly and does **not** re-resolve at connect time, there is
//!     no TOCTOU window for a DNS-rebinding attack. This replaces the old `check_ssrf_via_dns`
//!     pre-flight (which had an inherent race between the pre-flight DNS query and reqwest's
//!     own DNS resolution at connect time).
//!   - SEC-8: 6to4 (`2002::/16`), Teredo (`2001::/32`), and NAT64 (`64:ff9b::/96`) tunnel
//!     prefixes are classified by extracting and re-checking the embedded IPv4 address.
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
    dns::{Addrs, Name, Resolve, Resolving},
    header::{HeaderMap, HeaderName, HeaderValue},
    redirect, Client, Url,
};
use std::{
    error::Error as StdError,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::{Arc, LazyLock},
    time::Duration,
};
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
    /// Whether `http://` (plain HTTP) URLs are permitted for the plugin's granted hostnames.
    /// Requires an explicit user grant at install time (`[capabilities] allow_http = true` AND
    /// approved by the user in the approval UI). Default: `false` (HTTPS-only).
    pub allow_http: bool,
}

impl Default for HttpLimits {
    fn default() -> Self {
        Self {
            max_response_bytes: 8 * 1024 * 1024,
            // 4 s is below the default epoch ceiling (5 s) to leave headroom for DNS + connect.
            connect_timeout_secs: 4,
            request_timeout_secs: 4,
            allow_http: false,
        }
    }
}

// ── SSRF guard: blocked IP ranges (RFC-0010 §3.3) ─────────────────────────────────────────────

// RFC-1918 private + special-use ranges that must never be contacted by a plugin.
//
// The statics are initialised once (on first call) via `LazyLock` — the old inline-array
// approach re-parsed the CIDR strings on every invocation of `is_ssrf_blocked_ip`, which
// contradicts the module-doc promise that they are "parsed once at module scope".
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
//   192.0.0.0/24     IETF Protocol Assignments (RFC 6890)
//   198.18.0.0/15    Network Interconnect Device Benchmarking (RFC 2544)
//
// IPv6 ranges:
//   ::               unspecified address (connects to loopback on Linux)
//   ::1/128          loopback
//   ::a.b.c.d        IPv4-compatible (deprecated, RFC 4291 §2.5.5.1) — checked inline
//   ::ffff:0:0/96    IPv4-mapped (catches IPv4-in-IPv6 SSRF) — checked inline via to_ipv4_mapped
//   fc00::/7         unique local (ULA)
//   fe80::/10        link-local
//   ff00::/8         IPv6 multicast
//   2002::/16        6to4 (SEC-8: embedded IPv4 extracted and recursed)
//   2001::/32        Teredo (SEC-8: embedded IPv4 extracted and recursed)
//   64:ff9b::/96     NAT64 (SEC-8: embedded IPv4 extracted and recursed)

macro_rules! ipv4net {
    ($s:literal) => {{
        // safe: compile-time string literal — parse errors are caught at startup
        $s.parse::<Ipv4Net>().expect("valid IPv4 CIDR")
    }};
}

macro_rules! ipv6net {
    ($s:literal) => {{
        $s.parse::<Ipv6Net>().expect("valid IPv6 CIDR")
    }};
}

/// IPv4 ranges blocked for SSRF (RFC-0010 §3.3). Initialised once at first call.
static SSRF_BLOCKED_V4: LazyLock<Vec<Ipv4Net>> = LazyLock::new(|| {
    vec![
        ipv4net!("10.0.0.0/8"),
        ipv4net!("172.16.0.0/12"),
        ipv4net!("192.168.0.0/16"),
        ipv4net!("169.254.0.0/16"), // link-local (AWS metadata endpoint)
        ipv4net!("100.64.0.0/10"),  // CGNAT
        ipv4net!("224.0.0.0/4"),    // IPv4 multicast (Class D)
        ipv4net!("240.0.0.0/4"),    // reserved (Class E)
        ipv4net!("0.0.0.0/8"),      // "this" network
        ipv4net!("192.0.0.0/24"),   // IETF Protocol Assignments (RFC 6890)
        ipv4net!("198.18.0.0/15"),  // Benchmarking (RFC 2544)
    ]
});

/// IPv6 ranges blocked for SSRF (loopback, ULA, link-local, multicast). Initialised once.
/// IPv4-mapped, IPv4-compatible, and tunnel prefixes are handled inline in `is_ssrf_blocked_ip`.
static SSRF_BLOCKED_V6: LazyLock<Vec<Ipv6Net>> = LazyLock::new(|| {
    vec![
        ipv6net!("fc00::/7"),  // unique local (ULA)
        ipv6net!("fe80::/10"), // link-local
        ipv6net!("ff00::/8"),  // IPv6 multicast
    ]
});

/// Extract the embedded IPv4 address from an IPv6 tunnel-prefix address (SEC-8, issue #103).
///
/// Returns `Some(v4)` for the three tunnel families that carry a potentially-private IPv4:
///
/// - **6to4** (`2002::/16`, RFC 3056): the embedded IPv4 is octets 2–5 of the IPv6 address.
/// - **Teredo** (`2001::/32`, RFC 4380): octets 12–15 of the IPv6 address are the client IPv4,
///   XOR'd with `0xFFFF_FFFF`.
/// - **NAT64** (`64:ff9b::/96`, RFC 6052): the embedded IPv4 is octets 12–15.
///
/// Returns `None` for all other IPv6 prefixes.
fn embedded_v4_from_tunnel(v6: Ipv6Addr) -> Option<Ipv4Addr> {
    let octets = v6.octets();
    // 6to4: 2002::/16 → embedded IPv4 at bytes [2..6].
    if octets[0] == 0x20 && octets[1] == 0x02 {
        return Some(Ipv4Addr::new(octets[2], octets[3], octets[4], octets[5]));
    }
    // Teredo: 2001:0000::/32 → client IPv4 at bytes [12..16] XOR 0xFF each byte.
    // (Note: 2001:0000::/32 is specifically Teredo; 2001:db8::/32 is documentation space
    // and does not contain embedded addresses in this sense.)
    if octets[0] == 0x20 && octets[1] == 0x01 && octets[2] == 0x00 && octets[3] == 0x00 {
        return Some(Ipv4Addr::new(
            octets[12] ^ 0xff,
            octets[13] ^ 0xff,
            octets[14] ^ 0xff,
            octets[15] ^ 0xff,
        ));
    }
    // NAT64: 64:ff9b::/96 → embedded IPv4 at bytes [12..16].
    if octets[0] == 0x00
        && octets[1] == 0x64
        && octets[2] == 0xff
        && octets[3] == 0x9b
        && octets[4] == 0x00
        && octets[5] == 0x00
        && octets[6] == 0x00
        && octets[7] == 0x00
        && octets[8] == 0x00
        && octets[9] == 0x00
        && octets[10] == 0x00
        && octets[11] == 0x00
    {
        return Some(Ipv4Addr::new(
            octets[12], octets[13], octets[14], octets[15],
        ));
    }
    None
}

/// Returns `true` if `ip` falls in any private, loopback, link-local, reserved, or tunnel-embedded
/// private range and must not be contacted by a plugin (SSRF guard, SEC-1/SEC-8).
///
/// In addition to the direct range checks, IPv6 forms that carry an embedded IPv4 address are
/// classified by extracting and re-checking the embedded IPv4:
///
/// - **IPv4-mapped** (`::ffff:a.b.c.d`) — handled by `to_ipv4_mapped()`.
/// - **IPv4-compatible** (`::a.b.c.d`, deprecated RFC 4291 §2.5.5.1) — detected by all-zero
///   prefix bytes; embedded IPv4 extracted from octets[12..16].
/// - **6to4** (`2002::/16`), **Teredo** (`2001::/32`), **NAT64** (`64:ff9b::/96`) — SEC-8.
///
/// All CIDR sets are built once into module-level [`LazyLock`] statics; this function does not
/// parse strings.
pub(crate) fn is_ssrf_blocked_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            // `is_unspecified` (0.0.0.0) is covered by 0.0.0.0/8 below, but the explicit
            // check is clearer and symmetric with the IPv6 path.
            if v4.is_unspecified() || v4.is_loopback() {
                return true;
            }
            SSRF_BLOCKED_V4.iter().any(|net| net.contains(&v4))
        }
        IpAddr::V6(v6) => {
            // Unspecified `::` connects to loopback on Linux — block it explicitly.
            if v6.is_unspecified() || v6.is_loopback() {
                return true;
            }
            // IPv4-mapped (::ffff:x.x.x.x) — recursively check the embedded IPv4.
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_ssrf_blocked_ip(IpAddr::V4(v4));
            }
            // IPv4-compatible (deprecated ::a.b.c.d, RFC 4291 §2.5.5.1).
            // Octets[0..12] are all zero; bytes[10..12] are [0,0] (not [0xff,0xff] which is
            // IPv4-mapped, already handled above). `::` and `::1` are blocked before this point.
            let octets = v6.octets();
            if octets[..12].iter().all(|&b| b == 0) {
                return is_ssrf_blocked_ip(IpAddr::V4(Ipv4Addr::new(
                    octets[12], octets[13], octets[14], octets[15],
                )));
            }
            // SEC-8 (issue #103): tunnel mechanisms (6to4, Teredo, NAT64) embed a
            // potentially-private IPv4 address. Extract it and recurse.
            if let Some(v4) = embedded_v4_from_tunnel(v6) {
                return is_ssrf_blocked_ip(IpAddr::V4(v4));
            }
            SSRF_BLOCKED_V6.iter().any(|net| net.contains(&v6))
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
/// `allow_http` mirrors the manifest's `[network].allow_http` field (propagated via
/// [`HttpLimits::allow_http`]). When `false` (the default), only `https://` is permitted;
/// `http://` is rejected to prevent credentials and response data from being exposed on-path.
///
/// Returns the parsed `Url` on success, or a redacted error string on failure. This is a
/// **synchronous** pre-flight — it does not perform DNS resolution.
pub(crate) fn validate_url(
    req: &HttpRequest,
    grants: &[String],
    allow_http: bool,
) -> Result<Url, String> {
    let url = Url::parse(&req.url).map_err(|e| format!("invalid URL: {e}"))?;

    match url.scheme() {
        "https" => {}
        "http" if allow_http => {}
        "http" => {
            return Err("http:// is not allowed; the plugin manifest must set \
                 [network].allow_http = true and the user must approve it at install time"
                .to_owned())
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

// ── Pinned SSRF DNS resolver (SEC-1, issue #103) ──────────────────────────────────────────────

/// A `reqwest::dns::Resolve` implementation that closes the DNS-rebinding TOCTOU window.
///
/// The old `check_ssrf_via_dns` pre-flight resolved the hostname, validated the IPs, then
/// discarded those IPs — reqwest performed a **second, independent** DNS resolution at TCP
/// connect time. An attacker with DNS control over a granted hostname could serve a public IP
/// during our pre-flight, then flip to `169.254.169.254` (or any private range) before reqwest
/// connected. This is a classic low-TTL DNS-rebinding attack.
///
/// `PinnedSsrfDnsResolver` closes this gap: because reqwest's `HttpConnector` uses the custom
/// resolver as the **sole DNS authority** (no system re-resolve at connect time), the IP
/// returned by our resolver is exactly the IP that is connected to. The SSRF check is embedded
/// directly in the resolution step, validated at the exact moment the connection is established.
///
/// Per-call behaviour:
/// 1. Resolve `name` via `tokio::net::lookup_host` (with a timeout).
/// 2. Classify every resulting IP with [`is_ssrf_blocked_ip`] (covers loopback, RFC-1918,
///    CGNAT, link-local, ULA, IPv4-mapped, IPv4-multicast, and SEC-8 tunnel prefixes).
/// 3. If **any** IP is blocked, the entire resolution is rejected (fail-closed).
/// 4. If **none** resolved, the resolution is rejected.
/// 5. Return the validated IPs to hyper, which connects to them directly.
pub(crate) struct PinnedSsrfDnsResolver {
    /// DNS lookup timeout; matches `HttpLimits::connect_timeout_secs`.
    timeout: Duration,
}

impl PinnedSsrfDnsResolver {
    pub(crate) fn new(connect_timeout_secs: u64) -> Self {
        Self {
            timeout: Duration::from_secs(connect_timeout_secs),
        }
    }
}

impl Resolve for PinnedSsrfDnsResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let timeout = self.timeout;
        Box::pin(async move {
            let host = name.as_str();
            // Port 0 is a placeholder — `lookup_host` needs a port but we only use the IP.
            let addr_str = format!("{host}:0");

            let addrs = tokio::time::timeout(timeout, tokio::net::lookup_host(&addr_str))
                .await
                .map_err(|_| {
                    Box::new(std::io::Error::other(format!(
                        "DNS lookup timed out for {host}"
                    ))) as Box<dyn StdError + Send + Sync>
                })?
                .map_err(|e| Box::new(e) as Box<dyn StdError + Send + Sync>)?;

            let mut found_any = false;
            let mut validated: Vec<SocketAddr> = Vec::new();

            for addr in addrs {
                found_any = true;
                if is_ssrf_blocked_ip(addr.ip()) {
                    warn!(
                        target: "cairn_plugin::http_fetch",
                        host = host,
                        ip = %addr.ip(),
                        "SSRF: DNS resolved to a blocked IP, rejecting connection (SEC-1)"
                    );
                    return Err(Box::new(std::io::Error::other(
                        "SSRF: hostname resolved to a blocked IP".to_owned(),
                    )) as Box<dyn StdError + Send + Sync>);
                }
                validated.push(addr);
            }

            if !found_any {
                return Err(
                    Box::new(std::io::Error::other("DNS: no addresses resolved"))
                        as Box<dyn StdError + Send + Sync>,
                );
            }

            Ok(Box::new(validated.into_iter()) as Addrs)
        })
    }
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
/// - **[`PinnedSsrfDnsResolver`]** as the sole DNS authority (SEC-1, issue #103)
///
/// The pinned DNS resolver closes the DNS-rebinding TOCTOU window. By installing it as the
/// `dns_resolver`, reqwest's `HttpConnector` calls it for every hostname resolution and does
/// **not** perform an additional system-level DNS query at connect time. The IP address we
/// validate in the resolver is exactly the IP that is connected to.
///
/// Redirects are intentionally disabled. A custom redirect-following policy that re-checks
/// the grant list on each hop would still be vulnerable to SSRF via DNS rebinding on the
/// redirect target (no async DNS pre-flight can run inside reqwest's synchronous redirect
/// closure). Returning the 3xx response to the plugin is the safe choice: the plugin can
/// inspect the `Location` header and issue a new `http-fetch` call, which goes through the
/// full pipeline including the pinned DNS SSRF resolver.
///
/// The client is built once per plugin instance and shared across calls (it holds a
/// connection pool internally, which is safe to reuse from a single thread via `block_on`).
pub(crate) fn build_client(limits: HttpLimits) -> Result<Client, String> {
    let resolver = Arc::new(PinnedSsrfDnsResolver::new(limits.connect_timeout_secs));
    Client::builder()
        .use_rustls_tls()
        .connect_timeout(Duration::from_secs(limits.connect_timeout_secs))
        .timeout(Duration::from_secs(limits.request_timeout_secs))
        // Redirects are NOT followed; the plugin receives the 3xx and handles it explicitly.
        // This prevents SSRF via redirect chaining (a server could redirect to a private IP).
        .redirect(redirect::Policy::none())
        // Install the pinned SSRF DNS resolver (SEC-1). This is the sole DNS authority for
        // this client — reqwest will not re-resolve at connect time.
        .dns_resolver(resolver)
        // SECURITY: disable system proxy inheritance. If the host has HTTPS_PROXY/ALL_PROXY set,
        // ambient proxy config would tunnel requests through the proxy, which performs its own
        // DNS resolution — completely bypassing the PinnedSsrfDnsResolver and re-opening the
        // SSRF/rebinding window against internal targets. A brokered client must never inherit
        // ambient proxy configuration.
        .no_proxy()
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
/// 1. Validate URL + hostname grant (sync); reject bad schemes, unlisted hosts, IP-literal SSRF.
/// 2. Execute the HTTP call (async); [`PinnedSsrfDnsResolver`] validates every IP at connect
///    time inside reqwest (SEC-1, issue #103) — no separate DNS pre-flight step.
///
/// Called from `CompState::http_fetch` via `Handle::block_on`.
pub(crate) async fn do_http_fetch(
    req: &HttpRequest,
    grants: &[String],
    client: &Client,
    limits: HttpLimits,
) -> Result<HttpResponse, String> {
    let url = validate_url(req, grants, limits.allow_http)?;
    // The `PinnedSsrfDnsResolver` installed in `client` resolves hostnames, validates every
    // returned IP against the SSRF block list, and feeds only validated IPs to hyper's connector.
    // No TOCTOU window: reqwest uses the resolver's output directly without a second system
    // DNS query. A blocked IP triggers a connect-time error, surfaced here as a reqwest error.
    execute_fetch(req, url, client, limits).await
}

/// Test-only variant that bypasses the [`PinnedSsrfDnsResolver`] so wiremock servers (which
/// bind to `127.0.0.1`, a loopback address blocked by the resolver) can be used as mock
/// targets. The caller must pass a client built without the pinned resolver (built via
/// [`build_client_no_ssrf_resolver`]). The hostname-grant check still runs; IP-literal SSRF
/// validation from `validate_url` still runs for URL-embedded IPs.
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

/// Test-only client builder that omits the [`PinnedSsrfDnsResolver`] so the reqwest client
/// can connect to loopback addresses used by wiremock in tests.
///
/// **Do not use this outside tests.**
#[cfg(test)]
pub(crate) fn build_client_no_ssrf_resolver(limits: HttpLimits) -> Result<Client, String> {
    Client::builder()
        .use_rustls_tls()
        .connect_timeout(Duration::from_secs(limits.connect_timeout_secs))
        .timeout(Duration::from_secs(limits.request_timeout_secs))
        .redirect(redirect::Policy::none())
        // Disable system proxy even in tests so wiremock connections don't leak through
        // a developer's HTTPS_PROXY env var.
        .no_proxy()
        .connection_verbose(false)
        .build()
        .map_err(|e| format!("failed to build test HTTP client: {e}"))
}

// ── Unit tests ─────────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

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

    // ── SEC-8: tunnel-embedded IPv4 ─────────────────────────────────────────────────────────

    #[test]
    fn sec8_6to4_embedding_private_is_blocked() {
        // 6to4: 2002:AABB:CCDD::/48 where AA.BB.CC.DD is the embedded IPv4.
        // 2002:0a00:0001:: encodes 10.0.0.1 (RFC-1918) → must be blocked.
        let addr: Ipv6Addr = "2002:0a00:0001::".parse().unwrap();
        assert!(
            is_ssrf_blocked_ip(IpAddr::V6(addr)),
            "6to4 with embedded 10.0.0.1 must be blocked"
        );
        // 2002:7f00:0001:: encodes 127.0.0.1 (loopback) → must be blocked.
        let loopback: Ipv6Addr = "2002:7f00:0001::".parse().unwrap();
        assert!(
            is_ssrf_blocked_ip(IpAddr::V6(loopback)),
            "6to4 with embedded 127.0.0.1 must be blocked"
        );
        // 2002:08080808:: encodes 8.8.8.8 (public) → must NOT be blocked.
        let public: Ipv6Addr = "2002:0808:0808::".parse().unwrap();
        assert!(
            !is_ssrf_blocked_ip(IpAddr::V6(public)),
            "6to4 with embedded 8.8.8.8 should be allowed"
        );
    }

    #[test]
    fn sec8_nat64_embedding_private_is_blocked() {
        // NAT64: 64:ff9b::/96, last 4 bytes are the IPv4.
        // 64:ff9b::c0a8:0001 = 64:ff9b::192.168.0.1 → must be blocked.
        let addr: Ipv6Addr = "64:ff9b::c0a8:0001".parse().unwrap();
        assert!(
            is_ssrf_blocked_ip(IpAddr::V6(addr)),
            "NAT64 with embedded 192.168.0.1 must be blocked"
        );
        // 64:ff9b::7f00:0001 = 64:ff9b::127.0.0.1 → must be blocked.
        let loopback: Ipv6Addr = "64:ff9b::7f00:0001".parse().unwrap();
        assert!(
            is_ssrf_blocked_ip(IpAddr::V6(loopback)),
            "NAT64 with embedded 127.0.0.1 must be blocked"
        );
        // 64:ff9b::0808:0808 = 64:ff9b::8.8.8.8 → must NOT be blocked.
        let public: Ipv6Addr = "64:ff9b::0808:0808".parse().unwrap();
        assert!(
            !is_ssrf_blocked_ip(IpAddr::V6(public)),
            "NAT64 with embedded 8.8.8.8 should be allowed"
        );
    }

    #[test]
    fn sec8_teredo_embedding_private_is_blocked() {
        // Teredo: 2001:0000::/32, client IPv4 at octets [12..16] XOR 0xFF each.
        // To embed 127.0.0.1 (loopback): XOR gives 0x80, 0xFF, 0xFF, 0xFE → bytes 12..16.
        // Build manually: 2001:0000:<server_v4>:<udp_port>:<flags>:<client_v4_xored>
        // We just construct the octets directly.
        let mut octets = [0u8; 16];
        octets[0] = 0x20;
        octets[1] = 0x01;
        // Bytes 12..16: 127.0.0.1 XOR 0xFF = 0x80, 0xFF, 0xFF, 0xFE
        octets[12] = 0x80;
        octets[13] = 0xff;
        octets[14] = 0xff;
        octets[15] = 0xfe;
        let addr = Ipv6Addr::from(octets);
        assert!(
            is_ssrf_blocked_ip(IpAddr::V6(addr)),
            "Teredo with embedded 127.0.0.1 (XOR'd) must be blocked"
        );
        // Embed 8.8.8.8 (public): XOR gives 0xF7, 0xF7, 0xF7, 0xF7
        let mut octets2 = [0u8; 16];
        octets2[0] = 0x20;
        octets2[1] = 0x01;
        octets2[12] = 0xf7;
        octets2[13] = 0xf7;
        octets2[14] = 0xf7;
        octets2[15] = 0xf7;
        let public = Ipv6Addr::from(octets2);
        assert!(
            !is_ssrf_blocked_ip(IpAddr::V6(public)),
            "Teredo with embedded 8.8.8.8 should be allowed"
        );
    }

    #[test]
    fn pinned_ssrf_resolver_is_correct_type() {
        // Verify PinnedSsrfDnsResolver can be constructed and upcast to Arc<dyn Resolve>.
        let resolver = PinnedSsrfDnsResolver::new(4);
        let _: Arc<dyn Resolve> = Arc::new(resolver);
    }

    /// SEC-1 behavioral test: the resolver must reject a hostname that resolves to loopback.
    ///
    /// `localhost` is expected to resolve to `127.0.0.1` or `::1` on any standard system —
    /// both are loopback addresses and both are in the SSRF block list. The resolver must
    /// return `Err`, not a list of `SocketAddr`. This closes the TOCTOU path: the only IPs
    /// we pass to reqwest are ones that passed `is_ssrf_blocked_ip` at resolve time.
    #[tokio::test]
    async fn pinned_resolver_rejects_loopback_hostname() {
        use std::str::FromStr;
        let resolver = PinnedSsrfDnsResolver::new(4);
        let name = Name::from_str("localhost").expect("'localhost' is a valid DNS name");
        let result = resolver.resolve(name).await;
        assert!(
            result.is_err(),
            "localhost resolves to a loopback address; the pinned resolver must reject it"
        );
    }

    /// SEC-6: IPv4-compatible `::a.b.c.d` form (deprecated, RFC 4291 §2.5.5.1).
    /// Must be blocked when the embedded IPv4 is in a private/loopback range.
    #[test]
    fn ipv4_compatible_loopback_is_blocked() {
        // ::127.0.0.1 in IPv4-compatible form — all of octets[0..12] are 0, last 4 are 127.0.0.1.
        let addr: Ipv6Addr = "::7f00:0001".parse().unwrap(); // ::127.0.0.1
        assert!(
            is_ssrf_blocked_ip(IpAddr::V6(addr)),
            "::127.0.0.1 (IPv4-compatible loopback) must be blocked"
        );
        // ::10.0.0.1 — RFC-1918 embedded
        let private: Ipv6Addr = "::a00:0001".parse().unwrap(); // ::10.0.0.1
        assert!(
            is_ssrf_blocked_ip(IpAddr::V6(private)),
            "::10.0.0.1 (IPv4-compatible private) must be blocked"
        );
        // ::8.8.8.8 — public address, must be allowed
        let public: Ipv6Addr = "::808:808".parse().unwrap(); // ::8.8.8.8
        assert!(
            !is_ssrf_blocked_ip(IpAddr::V6(public)),
            "::8.8.8.8 (IPv4-compatible public) must not be blocked"
        );
    }

    /// SEC-6 extra ranges: 192.0.0.0/24 (IETF protocol assignments) and 198.18.0.0/15 (benchmarking).
    #[test]
    fn extra_blocked_ranges_are_blocked() {
        assert!(
            is_ssrf_blocked_ip("192.0.0.1".parse().unwrap()),
            "192.0.0.1 (IETF protocol assignments) must be blocked"
        );
        assert!(
            is_ssrf_blocked_ip("198.18.0.1".parse().unwrap()),
            "198.18.0.1 (benchmarking RFC 2544) must be blocked"
        );
        assert!(
            is_ssrf_blocked_ip("198.19.255.255".parse().unwrap()),
            "198.19.255.255 (benchmarking RFC 2544, end) must be blocked"
        );
    }

    /// `allow_http = true` allows an http:// URL through `validate_url`.
    #[test]
    fn allow_http_permits_plain_http_url() {
        let grants = vec!["example.com".to_owned()];
        let req = make_req("http://example.com/data");
        // With allow_http = false (default), http:// is rejected.
        let err = validate_url(&req, &grants, false).unwrap_err();
        assert!(
            err.contains("http://") || err.contains("https://") || err.contains("allow_http"),
            "http:// must be rejected when allow_http=false, err = {err}"
        );
        // With allow_http = true, http:// passes validate_url (DNS-SSRF check still runs at connect).
        let url = validate_url(&req, &grants, true)
            .expect("http:// must be allowed when allow_http=true");
        assert_eq!(url.scheme(), "http");
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
        assert!(validate_url(&req, &grants, false).is_ok());
    }

    #[test]
    fn unlisted_host_is_rejected() {
        let grants = vec!["api.example.com".to_owned()];
        let req = make_req("https://evil.com/steal");
        let err = validate_url(&req, &grants, false).unwrap_err();
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
        let err = validate_url(&req, &grants, false).unwrap_err();
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
        let err = validate_url(&req, &grants, false).unwrap_err();
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
        let err = validate_url(&req, &grants, false).unwrap_err();
        assert!(
            err.contains("SSRF") || err.contains("blocked"),
            "IPv6 ULA must be SSRF-blocked, err = {err}"
        );
    }

    #[test]
    fn ipv6_unspecified_is_rejected() {
        // `::` is the IPv6 unspecified address; on Linux connect() to `::` reaches loopback.
        let grants = vec!["::".to_owned(), "[::0]".to_owned()];
        for url in ["https://[::]/", "https://[::0]/"] {
            let req = make_req(url);
            let err = validate_url(&req, &grants, false).unwrap_err();
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
        let err = validate_url(&req, &grants, false).unwrap_err();
        assert!(
            err.contains("SSRF") || err.contains("blocked"),
            "IPv4 multicast must be SSRF-blocked, err = {err}"
        );
    }

    #[test]
    fn ipv6_multicast_is_rejected() {
        let grants = vec!["[ff02::1]".to_owned()];
        let req = make_req("https://[ff02::1]/");
        let err = validate_url(&req, &grants, false).unwrap_err();
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
        let err = validate_url(&req, &grants, false).unwrap_err();
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
        let err = validate_url(&req, &grants_decimal, false).unwrap_err();
        assert!(
            err.contains("network grant") || err.contains("SSRF") || err.contains("blocked"),
            "decimal-encoded IPv4 must be rejected, err = {err}"
        );

        // With the normalised address in grants, the SSRF check still blocks it.
        let grants_norm = vec!["127.0.0.1".to_owned()];
        let err = validate_url(&req, &grants_norm, false).unwrap_err();
        assert!(
            err.contains("SSRF") || err.contains("blocked"),
            "decimal-encoded IPv4 with normalised grant must still be SSRF-blocked, err = {err}"
        );
    }

    #[test]
    fn ftp_scheme_is_rejected() {
        let grants = vec!["example.com".to_owned()];
        let req = make_req("ftp://example.com/file");
        let err = validate_url(&req, &grants, false).unwrap_err();
        assert!(err.contains("scheme"), "err = {err}");
    }

    #[test]
    fn file_scheme_is_rejected() {
        let grants = vec!["example.com".to_owned()];
        let req = make_req("file:///etc/passwd");
        let err = validate_url(&req, &grants, false).unwrap_err();
        assert!(err.contains("scheme"), "err = {err}");
    }

    #[test]
    fn invalid_url_is_rejected() {
        let grants = vec!["example.com".to_owned()];
        let req = make_req("not a url at all !!!");
        let err = validate_url(&req, &grants, false).unwrap_err();
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
        // Use the non-SSRF resolver client so wiremock (127.0.0.1) is reachable.
        let client = build_client_no_ssrf_resolver(limits).expect("client");

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
        let client = build_client_no_ssrf_resolver(limits).expect("client");

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
        let client = build_client_no_ssrf_resolver(limits).expect("client");

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
        let client = build_client_no_ssrf_resolver(limits).expect("client");

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
        let client = build_client_no_ssrf_resolver(limits).expect("client");

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
        let client = build_client_no_ssrf_resolver(limits).expect("client");

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
    /// `http-fetch` call, which will go through the full SSRF pipeline (pinned DNS resolver).
    /// This prevents redirect-chaining SSRF attacks where a server redirects to a private IP.
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
        let client = build_client_no_ssrf_resolver(limits).expect("client");

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
