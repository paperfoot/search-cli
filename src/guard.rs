//! Network-safety guard for URL-input modes. The extract/scrape chain starts
//! with LOCAL fetches (direct, then stealth), so a prompt-injected agent could
//! otherwise point them at cloud metadata (169.254.169.254), localhost admin
//! panels, or LAN hosts. Default-deny anything non-public; `--allow-private`
//! overrides.

use crate::errors::SearchError;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Set once from `--allow-private` before a search runs. The local rungs read
/// it to pick their client, so the flag reaches every redirect hop, not only
/// the pre-flight check. Off (guarded) unless set.
static ALLOW_PRIVATE: AtomicBool = AtomicBool::new(false);

/// Record the `--allow-private` flag for this process. `main` calls it once
/// the search arguments are parsed (integration handover from cli-extract).
#[cfg_attr(not(test), allow(dead_code))]
pub fn set_allow_private(allow: bool) {
    ALLOW_PRIVATE.store(allow, Ordering::Relaxed);
}

/// True when the user passed `--allow-private`.
pub fn allow_private() -> bool {
    ALLOW_PRIVATE.load(Ordering::Relaxed)
}

/// Reject URLs whose host is (or resolves to) a non-public address: the
/// early, friendly refusal before any rung runs. It is not the enforcement:
/// a second lookup may answer differently (DNS rebinding), so the local
/// rungs' own resolvers (`page::resolve_public`, used by direct's reqwest and
/// stealth's wreq) check the addresses they connect to and fail closed. A
/// DNS failure here is therefore not a rejection; the fetch fails on its own.
pub async fn assert_public_url(raw: &str) -> Result<(), SearchError> {
    let parsed = url::Url::parse(raw).map_err(|e| SearchError::InvalidInput {
        message: format!("not a valid URL: {e}"),
    })?;

    let host = match parsed.host() {
        Some(h) => h,
        None => {
            return Err(SearchError::InvalidInput {
                message: "URL has no host".to_string(),
            })
        }
    };

    match host {
        url::Host::Ipv4(ip) => reject_if_private(IpAddr::V4(ip), raw)?,
        url::Host::Ipv6(ip) => reject_if_private(IpAddr::V6(ip), raw)?,
        url::Host::Domain(name) => {
            let lower = name.to_ascii_lowercase();
            if lower == "localhost"
                || lower.ends_with(".localhost")
                || lower.ends_with(".local")
                || lower.ends_with(".internal")
            {
                return Err(blocked(&lower));
            }
            let port = parsed.port_or_known_default().unwrap_or(443);
            let lookup = tokio::time::timeout(
                Duration::from_secs(3),
                tokio::net::lookup_host((lower.as_str(), port)),
            )
            .await;
            if let Ok(Ok(addrs)) = lookup {
                for addr in addrs {
                    reject_if_private(addr.ip(), &lower)?;
                }
            }
        }
    }
    Ok(())
}

/// Reject a bare hostname that resolves to a non-public address.
///
/// `search verify` opens a TCP connection to whatever host a domain's MX
/// record names. An attacker who controls a domain can point its MX at
/// 169.254.169.254 or a LAN address and use the CLI as a port scanner, so the
/// same default-deny that guards extract/scrape has to guard this too.
pub async fn assert_public_host(host: &str, port: u16) -> Result<(), SearchError> {
    let lower = host.to_ascii_lowercase();
    if let Ok(ip) = lower.parse::<IpAddr>() {
        return reject_if_private(ip, &lower);
    }
    if lower == "localhost"
        || lower.ends_with(".localhost")
        || lower.ends_with(".local")
        || lower.ends_with(".internal")
    {
        return Err(blocked(&lower));
    }
    let lookup = tokio::time::timeout(
        Duration::from_secs(3),
        tokio::net::lookup_host((lower.as_str(), port)),
    )
    .await;
    if let Ok(Ok(addrs)) = lookup {
        for addr in addrs {
            reject_if_private(addr.ip(), &lower)?;
        }
    }
    Ok(())
}

fn reject_if_private(ip: IpAddr, shown: &str) -> Result<(), SearchError> {
    if is_private(ip) {
        return Err(blocked(shown));
    }
    Ok(())
}

fn blocked(target: &str) -> SearchError {
    SearchError::InvalidInput {
        message: format!(
            "'{target}' is a private/loopback/link-local address — blocked by default so agents can't be steered into internal endpoints. Pass --allow-private to override."
        ),
    }
}

fn is_private(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_private_v4(v4),
        IpAddr::V6(v6) => {
            let s = v6.segments();
            v6.is_loopback()
                || v6.is_unspecified()
                || (s[0] & 0xfe00) == 0xfc00 // unique-local fc00::/7
                || (s[0] & 0xffc0) == 0xfe80 // link-local fe80::/10
                || (s[0] & 0xffc0) == 0xfec0 // deprecated site-local fec0::/10
                || embedded_v4(v6).is_some_and(is_private_v4)
        }
    }
}

fn is_private_v4(v4: Ipv4Addr) -> bool {
    let [a, b, c, _] = v4.octets();
    v4.is_loopback()
        || v4.is_private()
        || v4.is_link_local() // includes 169.254.169.254 metadata
        || v4.is_unspecified()
        || v4.is_broadcast()
        || a == 0 // "this network" 0/8
        || a == 100 && (64..128).contains(&b) // CGNAT 100.64/10
        || a == 198 && (b == 18 || b == 19) // benchmarking 198.18/15
        || a >= 240 // reserved 240/4
        || (a, b, c) == (192, 0, 0) // IETF 192.0.0/24 (Oracle Cloud metadata 192.0.0.192)
        || (a, b, c) == (192, 0, 2) // TEST-NET-1
        || (a, b, c) == (198, 51, 100) // TEST-NET-2
        || (a, b, c) == (203, 0, 113) // TEST-NET-3
        || (a, b, c) == (192, 88, 99) // 6to4 relay anycast
}

/// The IPv4 address carried inside an IPv6 one, in the forms a request can
/// still reach it through: IPv4-mapped (::ffff:a.b.c.d), the deprecated
/// IPv4-compatible (::a.b.c.d), NAT64 (64:ff9b::/96 and 64:ff9b:1::/48) and
/// 6to4 (2002:AABB:CCDD::/16).
fn embedded_v4(v6: Ipv6Addr) -> Option<Ipv4Addr> {
    let s = v6.segments();
    let tail =
        |hi: u16, lo: u16| Ipv4Addr::new((hi >> 8) as u8, hi as u8, (lo >> 8) as u8, lo as u8);
    if let Some(m) = v6.to_ipv4_mapped() {
        return Some(m);
    }
    match s {
        // ::a.b.c.d (but not :: or ::1, handled above)
        [0, 0, 0, 0, 0, 0, hi, lo] if hi != 0 => Some(tail(hi, lo)),
        [0x64, 0xff9b, 0, 0, 0, 0, hi, lo] => Some(tail(hi, lo)),
        [0x64, 0xff9b, 1, ..] => Some(tail(s[6], s[7])),
        [0x2002, hi, lo, ..] => Some(tail(hi, lo)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn ok(u: &str) -> bool {
        assert_public_url(u).await.is_ok()
    }

    /// `search verify` connects to whatever host a domain's MX record names,
    /// which is input the attacker controls. Same default-deny as URL modes.
    #[tokio::test]
    async fn mx_hosts_are_checked_before_smtp_connects() {
        let blocked =
            |h: &'static str| async move { crate::guard::assert_public_host(h, 25).await.is_err() };
        assert!(blocked("169.254.169.254").await, "cloud metadata");
        assert!(blocked("127.0.0.1").await, "loopback");
        assert!(blocked("10.0.0.5").await, "private");
        assert!(blocked("192.168.1.1").await, "private");
        assert!(blocked("mail.internal").await, "internal suffix");
        assert!(blocked("localhost").await);
        assert!(blocked("::1").await, "ipv6 loopback");
        // A real MX host must still be reachable, or the feature is dead.
        assert!(crate::guard::assert_public_host("aspmx.l.google.com", 25)
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn blocks_metadata_loopback_private_and_internal_names() {
        assert!(!ok("http://169.254.169.254/latest/meta-data/").await);
        assert!(!ok("http://127.0.0.1:8080/admin").await);
        assert!(!ok("http://10.0.0.5/").await);
        assert!(!ok("http://192.168.1.1/").await);
        assert!(!ok("http://100.100.1.1/").await); // CGNAT / tailscale range
        assert!(!ok("http://localhost/x").await);
        assert!(!ok("http://router.local/").await);
        assert!(!ok("http://db.prod.internal/").await);
        assert!(!ok("http://[::1]/").await);
        assert!(!ok("http://[fe80::1]/").await);
        assert!(!ok("http://[::ffff:127.0.0.1]/").await);
    }

    /// Review P11: private IPv4 reached through an IPv6 form, and the
    /// reserved v4 ranges, are refused.
    #[test]
    fn embedded_and_reserved_ranges_are_private() {
        let private = |s: &str| is_private(s.parse().unwrap());
        assert!(private("::7f00:1"), "IPv4-compatible loopback");
        assert!(private("64:ff9b::a9fe:a9fe"), "NAT64 metadata");
        assert!(private("64:ff9b:1::a00:1"), "local-use NAT64 10.0.0.1");
        assert!(private("2002:c0a8:101::1"), "6to4 192.168.1.1");
        assert!(private("fec0::1"), "site-local");
        assert!(private("198.18.0.1"));
        assert!(private("240.0.0.1"));
        assert!(private("0.1.2.3"));
        // IETF and documentation ranges; 192.0.0.192 is Oracle Cloud metadata.
        for ip in [
            "192.0.0.192",
            "192.0.2.1",
            "198.51.100.7",
            "203.0.113.9",
            "192.88.99.1",
        ] {
            assert!(private(ip), "{ip}");
        }
        assert!(!private("192.0.1.1") && !private("192.88.98.1"));
        assert!(!private("64:ff9b::808:808"), "NAT64 of 8.8.8.8 is public");
        assert!(!private("2002:808:808::1"), "6to4 of 8.8.8.8 is public");
        assert!(!private("2606:4700::1111"));
        assert!(!private("8.8.8.8"));
    }

    #[tokio::test]
    async fn allows_public_addresses() {
        assert!(ok("https://1.1.1.1/").await);
        assert!(ok("https://example.com/article").await);
    }
}
