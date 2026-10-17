//! SSRF guard: reject URLs/IPs that resolve to private or loopback ranges.
//!
//! Shared by the feed-fetch path (fetcher.rs) and the media-download path
//! (media.rs). Extracted from media.rs (batch-A architecture refactor) so the
//! fetcher no longer depends on the media module — dependency direction is
//! now one-way (fetcher → ssrf, media → ssrf).

/// Outcome of the SSRF pre-check. Distinguishing "confirmed private" from
/// "DNS lookup failed" matters at the feed-fetch entry: reporting a transient
/// DNS blip as "blocked: private/loopback address" makes it unmatchable by
/// the transient-error classifier, so a network-wide DNS outage would count
/// toward auto-disable on every source. LookupFailed is still blocked here
/// (fail-closed: reqwest may resolve differently on retry) but carries a
/// retryable error text.
#[derive(Debug, PartialEq, Eq)]
pub enum SsrfOutcome {
    /// Host resolves only to public addresses (or is a public literal IP).
    Allowed,
    /// Host resolves to a private/loopback/link-local address, is a private
    /// literal IP, has no host, or uses a non-http(s) scheme.
    Private,
    /// DNS resolution failed (transient): cannot confirm the target.
    LookupFailed,
}

/// Three-state SSRF verdict. See SsrfOutcome.
pub async fn ssrf_check(url: &str) -> SsrfOutcome {
    let Ok(u) = url::Url::parse(url) else { return SsrfOutcome::Private };
    let Some(host) = u.host_str() else { return SsrfOutcome::Private };
    // Only http/https allowed; other schemes (file, ftp…) blocked.
    if u.scheme() != "http" && u.scheme() != "https" {
        return SsrfOutcome::Private;
    }
    // Literal IP: check directly.
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        return if is_private_ip(&ip) { SsrfOutcome::Private } else { SsrfOutcome::Allowed };
    }
    // Hostname: async DNS lookup, check all returned addresses.
    let addr_str = format!("{}:{}", host, u.port_or_known_default().unwrap_or(443));
    let addrs: Vec<std::net::SocketAddr> = match tokio::net::lookup_host(&addr_str).await {
        Ok(iter) => iter.collect(),
        // DNS resolution failure: fail-closed reject (don't allow through
        // when we can't confirm whether the address is private), but report
        // it distinctly so callers can treat it as transient.
        Err(_) => return SsrfOutcome::LookupFailed,
    };
    if addrs.iter().any(|sa| is_private_ip(&sa.ip())) {
        SsrfOutcome::Private
    } else {
        SsrfOutcome::Allowed
    }
}

/// SSRF guard: reject URLs whose host resolves to a private/loopback address.
/// Covers 127/8, 10/8, 172.16/12, 192.168/16, 169.254/16, 100.64/10, ::1, fc00::/7.
/// Shared by the feed-fetch path and the media download path
/// (same private-address determination for both). DNS failures fold into
/// `true` (fail-closed) — callers wanting the three-state verdict (to retry
/// transient lookups) use ssrf_check directly.
pub async fn is_private_url(url: &str) -> bool {
    matches!(ssrf_check(url).await, SsrfOutcome::Private | SsrfOutcome::LookupFailed)
}

pub fn is_private_ip(ip: &std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => {
            let o = v4.octets();
            o[0] == 127                          // 127/8 loopback
                || o[0] == 10                    // 10/8
                || (o[0] == 172 && o[1] >= 16 && o[1] <= 31) // 172.16/12
                || (o[0] == 192 && o[1] == 168)  // 192.168/16
                || (o[0] == 169 && o[1] == 254)  // 169.254/16 link-local
                || o[0] == 0                     // 0.0.0.0/8
                || (o[0] == 100 && o[1] >= 64 && o[1] <= 127) // 100.64/10 CGNAT
                || (o[0] == 198 && (o[1] == 18 || o[1] == 19)) // 198.18/15 benchmarking
                || o[0] >= 224                   // 224/4 multicast + 240/4 reserved + broadcast
        }
        std::net::IpAddr::V6(v6) => {
            // IPv4-mapped (::ffff:x.x.x.x) and IPv4-compatible (::x.x.x.x):
            // to_ipv4() covers both forms; convert back to V4 before checking
            // to prevent bypasses.
            if let Some(v4) = v6.to_ipv4() {
                return is_private_ip(&std::net::IpAddr::V4(v4));
            }
            let seg = v6.segments();
            v6.is_loopback()                     // ::1
                || (seg[0] & 0xfe00) == 0xfc00   // fc00::/7 ULA
                || v6.is_unspecified()
                || (seg[0] & 0xffc0) == 0xfe80   // fe80::/10 link-local
                || seg[0] == 0x2002              // 2002::/16 6to4 (embeds arbitrary IPv4)
                || seg[0] == 0x2001 && seg[1] == 0 // 2001::/32 Teredo (embeds arbitrary IPv4)
        }
    }
}
