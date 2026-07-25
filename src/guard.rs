//! Network-safety guard for URL-input modes. The extract/scrape chain starts
//! with the LOCAL stealth scraper, so a prompt-injected agent could otherwise
//! point it at cloud metadata (169.254.169.254), localhost admin panels, or
//! LAN hosts. Default-deny anything non-public; `--allow-private` overrides.

use crate::errors::SearchError;
use std::net::IpAddr;
use std::time::Duration;

/// Reject URLs whose host is (or resolves to) a non-public address.
/// DNS failure is NOT a rejection — the scraper will fail on its own, with a
/// clearer error than a pre-flight guess. Resolution is checked once here;
/// DNS-rebinding between check and fetch is out of scope and documented.
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
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local() // includes 169.254.169.254 metadata
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.octets()[0] == 100 && (64..128).contains(&v4.octets()[1]) // CGNAT 100.64/10
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                || (v6.segments()[0] & 0xfe00) == 0xfc00 // unique-local fc00::/7
                || (v6.segments()[0] & 0xffc0) == 0xfe80 // link-local fe80::/10
                || v6.to_ipv4_mapped().is_some_and(|m| is_private(IpAddr::V4(m)))
        }
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

    #[tokio::test]
    async fn allows_public_addresses() {
        assert!(ok("https://1.1.1.1/").await);
        assert!(ok("https://example.com/article").await);
    }
}
