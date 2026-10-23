//! SSRF guard: reject URLs/IPs that resolve to private or loopback ranges.
//!
//! Shared by the feed-fetch path (fetcher.rs) and the media-download path
//! (media.rs). Extracted from media.rs (batch-A architecture refactor) so the
//! fetcher no longer depends on the media module — dependency direction is
//! now one-way (fetcher → ssrf, media → ssrf).
//!
//! DNS-rebinding defence: direct-connection clients install
//! [`GuardedResolver`] as their DNS resolver, so the addresses reqwest
//! actually connects to are the very addresses the guard vetted — closing the
//! time-of-check/time-of-use window between the pre-check and the connection.
//! Proxy-routed clients must NOT use it: the connection is built on the proxy
//! side, so a local resolver never sees the target lookup.

use std::net::{IpAddr, SocketAddr};

use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use tracing::warn;

/// Error text returned by [`GuardedResolver`] when the connection-layer
/// check rejects a lookup. Contains the substring "dns resolve failed" —
/// sendpolicy::is_net_error classifies such fetch errors as transient
/// (same substring contract as the pre-check branches in fetcher.rs).
pub(crate) const GUARD_RESOLVE_FAILED: &str = "dns resolve failed by SSRF guard";

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
    /// The URL could not be parsed or has no host: blocked, but reported
    /// separately so callers do not label a malformed media URL as a private
    /// address.
    InvalidUrl,
}

/// Resolve `host:port` and classify every returned address. Shared by the
/// pre-check ([`ssrf_check`]) and the connection-layer [`GuardedResolver`].
async fn lookup_classify(host: &str, port: u16) -> (SsrfOutcome, Vec<SocketAddr>) {
    let addr_str = format!("{}:{}", host, port);
    // Bind the match result before returning: the lookup temporary (which
    // borrows addr_str) must drop here, not at the end of the block, or the
    // local falls out of scope while still borrowed (E0597).
    let outcome = match tokio::net::lookup_host(&addr_str).await {
        Ok(iter) => {
            let addrs: Vec<SocketAddr> = iter.collect();
            (classify_addrs(&addrs), addrs)
        }
        // DNS resolution failure: fail-closed reject (don't allow through
        // when we can't confirm whether the address is private), but report
        // it distinctly so callers can treat it as transient.
        Err(_) => (SsrfOutcome::LookupFailed, Vec::new()),
    };
    outcome
}

/// Pure classification of a resolved address set: any private/loopback answer
/// vetoes the whole set (fail-closed — reqwest may try the next address).
fn classify_addrs(addrs: &[SocketAddr]) -> SsrfOutcome {
    if addrs.iter().any(|sa| is_private_ip(&sa.ip())) {
        SsrfOutcome::Private
    } else {
        SsrfOutcome::Allowed
    }
}

/// Three-state SSRF verdict. See SsrfOutcome.
pub async fn ssrf_check(url: &str) -> SsrfOutcome {
    let Ok(u) = url::Url::parse(url) else { return SsrfOutcome::InvalidUrl };
    // Only http/https allowed; other schemes (file, ftp, ws…) blocked. Check
    // the scheme before host_str: file URLs parse without a host and would
    // otherwise be reported as an invalid URL instead of a blocked scheme.
    if u.scheme() != "http" && u.scheme() != "https" {
        return SsrfOutcome::Private;
    }
    let Some(host) = u.host_str() else { return SsrfOutcome::InvalidUrl };
    // Literal IP: check directly.
    if let Ok(ip) = host.parse::<IpAddr>() {
        return if is_private_ip(&ip) {
            SsrfOutcome::Private
        } else {
            SsrfOutcome::Allowed
        };
    }
    // Hostname: async DNS lookup, check all returned addresses.
    let port = u.port_or_known_default().unwrap_or(443);
    lookup_classify(host, port).await.0
}

/// Connection-layer DNS guard closing the rebinding window of the pre-check.
///
/// The pre-check resolves the hostname and vetoes private answers, but reqwest
/// used to resolve again when connecting: a hostile DNS could answer the first
/// lookup with a public IP and the second with 127.0.0.1 (classic
/// time-of-check/time-of-use rebinding). Installing this resolver on a
/// direct-connection client makes the connection path itself re-resolve and
/// vet: an answer containing any private/loopback address fails the whole
/// lookup (the error text carries "dns resolve failed" so is_net_error keeps
/// treating transient blips as retryable). It re-resolves per connection
/// rather than pinning a cached IP, so ordinary DNS failover and CDN rotation
/// keep working.
///
/// Only for clients WITHOUT a proxy: through RSS_PROXY the connection target
/// is resolved on the proxy side and this resolver is never invoked (the pre-
/// check is skipped there for the same reason).
#[derive(Debug, Clone, Copy)]
pub struct GuardedResolver;

impl Resolve for GuardedResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_string();
        Box::pin(async move {
            // Port 0: reqwest substitutes the scheme's conventional port (or
            // the URL's explicit port) on the returned addresses.
            let (outcome, addrs) = lookup_classify(&host, 0).await;
            match outcome {
                SsrfOutcome::Allowed => {
                    let boxed: Addrs = Box::new(addrs.into_iter());
                    Ok(boxed)
                }
                // Private answer or failed lookup: reject. Fail-closed — the
                // caller never reaches a connection with an unvetted address.
                // io::Error mirrors hyper-util's GaiResolver error type; the
                // message keeps the "dns resolve failed" substring so
                // sendpolicy::is_net_error classifies DNS blips as transient.
                SsrfOutcome::Private | SsrfOutcome::LookupFailed | SsrfOutcome::InvalidUrl => {
                    warn!(host = %host, "ssrf: connection-layer DNS guard rejected lookup");
                    Err(Box::new(std::io::Error::other(GUARD_RESOLVE_FAILED))
                        as Box<dyn std::error::Error + Send + Sync>)
                }
            }
        })
    }
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
            // NAT64: 64:ff9b::/96 embeds the IPv4 target in the last 32 bits.
            // The local-use range 64:ff9b:1::/48 has no fixed embedding, so
            // block the whole 64:ff9b::/32 prefix outright.
            if seg[0] == 0x0064 && seg[1] == 0xff9b {
                if seg[2] == 0 && seg[3] == 0 && seg[4] == 0 && seg[5] == 0 {
                    let v4 = std::net::Ipv4Addr::new(
                        (seg[6] >> 8) as u8,
                        seg[6] as u8,
                        (seg[7] >> 8) as u8,
                        seg[7] as u8,
                    );
                    return is_private_ip(&std::net::IpAddr::V4(v4));
                }
                return true;
            }
            v6.is_loopback()                     // ::1
                || (seg[0] & 0xfe00) == 0xfc00   // fc00::/7 ULA
                || v6.is_unspecified()
                || (seg[0] & 0xffc0) == 0xfe80   // fe80::/10 link-local
                || (seg[0] & 0xff00) == 0xff00   // ff00::/8 multicast
                || seg[0] == 0x2002              // 2002::/16 6to4 (embeds arbitrary IPv4)
                || seg[0] == 0x2001 && seg[1] == 0 // 2001::/32 Teredo (embeds arbitrary IPv4)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn sa(ip: &str, port: u16) -> SocketAddr {
        SocketAddr::new(ip.parse::<IpAddr>().unwrap(), port)
    }

    #[test]
    fn classify_all_public_allowed() {
        assert_eq!(classify_addrs(&[sa("93.184.216.34", 0), sa("2606:2800:220:1:248:1893:25c8:1946", 0)]), SsrfOutcome::Allowed);
    }

    #[test]
    fn classify_any_private_vetoes() {
        // Rebinding shape: a mixed answer must fail closed, not pick the public entry.
        assert_eq!(classify_addrs(&[sa("93.184.216.34", 0), sa("127.0.0.1", 0)]), SsrfOutcome::Private);
        assert_eq!(classify_addrs(&[sa("169.254.169.254", 0)]), SsrfOutcome::Private);
        assert_eq!(classify_addrs(&[sa("::1", 0)]), SsrfOutcome::Private);
    }

    #[test]
    fn nat64_embedded_ipv4_and_multicast_blocked() {
        // 64:ff9b::/96 embeds the IPv4 target in the last 32 bits.
        assert!(is_private_ip(&"64:ff9b::a9fe:a9fe".parse().unwrap())); // 169.254.169.254
        assert!(is_private_ip(&"64:ff9b::7f00:1".parse().unwrap())); // 127.0.0.1
        assert!(!is_private_ip(&"64:ff9b::808:808".parse().unwrap())); // 8.8.8.8 public
        // Local-use NAT64 prefix has no fixed embedding: blocked wholesale.
        assert!(is_private_ip(&"64:ff9b:1::1".parse().unwrap()));
        // IPv6 multicast.
        assert!(is_private_ip(&"ff02::1".parse().unwrap()));
    }

    #[tokio::test]
    async fn resolver_rejects_loopback_hostname() {
        // localhost always resolves to loopback on any host. (No expect_err:
        // the Ok payload Addrs has no Debug impl.)
        let res = GuardedResolver.resolve(Name::from_str("localhost").unwrap()).await;
        let err = res.err().expect("loopback answer must be rejected");
        assert!(
            err.to_string().contains("dns resolve failed"),
            "error text must keep the is_net_error substring, got: {err}"
        );
    }

    #[tokio::test]
    async fn resolver_rejects_private_literal_ip() {
        // Literal IPs skip DNS in hyper-util's connector, but a hostname that
        // is an IP string still goes through lookup_host → private → veto.
        let res = GuardedResolver.resolve(Name::from_str("10.0.0.1").unwrap()).await;
        let err = res.err().expect("private literal must be rejected");
        assert!(err.to_string().contains("dns resolve failed"));
    }

    #[tokio::test]
    async fn resolver_never_leaks_private_for_unresolvable_name() {
        // Fail-closed invariant. A `.invalid` lookup normally errors (rejected);
        // on DNS-hijacking resolvers it may answer with a wildcard IP, in which
        // case the guard must pass only public addresses, never a private one.
        match GuardedResolver.resolve(Name::from_str("no-such-host.invalid").unwrap()).await {
            Err(err) => assert!(err.to_string().contains("dns resolve failed")),
            Ok(mut addrs) => assert!(addrs.all(|a| !is_private_ip(&a.ip()))),
        }
    }

    #[test]
    fn ssrf_check_rejects_non_http_scheme() {
        // Blocking decision is synchronous scheme parsing; safe to test without DNS.
        // (async fn needs a runtime; use a minimal one inline)
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        rt.block_on(async {
            assert_eq!(ssrf_check("file:///etc/passwd").await, SsrfOutcome::Private);
            assert_eq!(ssrf_check("ws://example.com").await, SsrfOutcome::Private);
            // Some non-special schemes fail Url::parse outright (reported as
            // InvalidUrl): either way they must never be Allowed.
            assert_ne!(ssrf_check("gopher://example.com").await, SsrfOutcome::Allowed);
            assert_eq!(ssrf_check("not a url").await, SsrfOutcome::InvalidUrl);
        });
    }
}
