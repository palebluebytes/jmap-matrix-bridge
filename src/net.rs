//! Outbound-HTTP safety helpers.
//!
//! Centralizes two concerns for requests the bridge makes to hosts it does not
//! fully control: sane timeouts (so a slow or hostile peer can't pin a task
//! indefinitely), and SSRF protection (so an attacker-supplied URL — e.g. a
//! remote `<img src>` in an inbound email — cannot be used to reach loopback,
//! private, or link-local addresses such as the cloud-metadata endpoint).

use std::net::IpAddr;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use reqwest::Url;

/// Connect-phase timeout (the key protection against an unreachable host).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Overall timeout for first-party calls. Generous because the same client
/// streams media (up to ~50 MB) and runs 30 s `/sync` long-polls; the connect
/// timeout bounds the common hang.
const CLIENT_TIMEOUT: Duration = Duration::from_secs(120);
/// Overall timeout for attacker-influenced fetches (small, capped bodies).
const FETCH_TIMEOUT: Duration = Duration::from_secs(30);

/// A `reqwest` client with sane connect/overall timeouts.
///
/// For first-party calls (the Matrix homeserver, double-puppet login/sync) where
/// the host is trusted but we still want to bound how long a stalled connection
/// can hold a task.
#[must_use]
pub fn client_with_timeouts() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(CLIENT_TIMEOUT)
        .connect_timeout(CONNECT_TIMEOUT)
        .build()
        // Only fails if the TLS backend can't initialize — unrecoverable and
        // would have failed everywhere else. Surface it rather than silently
        // returning a client without the timeouts this function guarantees.
        .expect("reqwest TLS backend failed to initialize")
}

/// Reject a URL that does not resolve to a routable public address.
///
/// Requires an `http`/`https` scheme, resolves the host, and fails if ANY
/// resolved IP is loopback, private, link-local (incl. `169.254.169.254`),
/// CGNAT, unspecified, multicast, or an IPv6 unique-/link-local address. Call
/// this before fetching any attacker-influenced URL — and re-check across
/// redirects (see [`safe_get`]).
pub async fn assert_public_url(url: &Url) -> Result<()> {
    let scheme = url.scheme();
    ensure!(
        scheme == "http" || scheme == "https",
        "unsupported URL scheme: {scheme}"
    );
    let host = url.host_str().context("URL has no host")?;
    let port = url.port_or_known_default().unwrap_or(80);

    let mut resolved = false;
    for addr in tokio::net::lookup_host((host, port))
        .await
        .with_context(|| format!("could not resolve host {host}"))?
    {
        resolved = true;
        ensure!(
            is_public(addr.ip()),
            "URL host resolves to a non-public address"
        );
    }
    ensure!(resolved, "host {host} did not resolve to any address");
    Ok(())
}

/// True only for addresses that are safe to dial from the server. Conservative:
/// anything special-purpose (loopback/private/link-local/CGNAT/multicast/…) is
/// rejected.
fn is_public(ip: IpAddr) -> bool {
    // Tests fetch from loopback mock servers; this override (compiled out of
    // release builds) lets them opt into allowing private/loopback addresses.
    #[cfg(test)]
    {
        if test_support::private_allowed() {
            return true;
        }
    }
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            !(v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_multicast()
                || v4.is_documentation()
                // Carrier-grade NAT: 100.64.0.0/10
                || (a == 100 && (b & 0xc0) == 64))
        }
        IpAddr::V6(v6) => {
            // IPv4-mapped (::ffff:a.b.c.d) — re-check the embedded v4.
            if let Some(mapped) = v6.to_ipv4_mapped() {
                return is_public(IpAddr::V4(mapped));
            }
            let first = v6.segments()[0];
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                // Unique-local fc00::/7
                || (first & 0xfe00) == 0xfc00
                // Link-local fe80::/10
                || (first & 0xffc0) == 0xfe80)
        }
    }
}

/// GET a URL that may be attacker-controlled, validating SSRF safety on every hop.
///
/// Redirects are followed manually (up to `max_redirects`) so each `Location` is
/// re-checked with [`assert_public_url`] — a public URL cannot 30x-redirect into
/// an internal one.
pub async fn safe_get(url: &str, max_redirects: u8) -> Result<reqwest::Response> {
    let client = reqwest::Client::builder()
        .timeout(FETCH_TIMEOUT)
        .connect_timeout(CONNECT_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .context("failed to build HTTP client")?;

    let mut current = Url::parse(url).context("invalid URL")?;
    for _ in 0..=max_redirects {
        assert_public_url(&current).await?;
        let resp = client.get(current.clone()).send().await?;
        if resp.status().is_redirection() {
            let location = resp
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .context("redirect without a valid Location header")?;
            current = current
                .join(location)
                .context("invalid redirect Location")?;
            continue;
        }
        return Ok(resp);
    }
    bail!("too many redirects")
}

/// Test-only override so unit tests can fetch from loopback mock servers without
/// the SSRF guard rejecting them. Entirely compiled out of release builds.
#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::atomic::{AtomicBool, Ordering};

    static ALLOW_PRIVATE: AtomicBool = AtomicBool::new(false);

    pub fn private_allowed() -> bool {
        ALLOW_PRIVATE.load(Ordering::SeqCst)
    }

    /// Permit private/loopback addresses in SSRF checks for the lifetime of the
    /// returned guard; resets on drop. Bind it (`let _g = …`) for the test body.
    #[must_use]
    pub fn allow_private() -> Guard {
        ALLOW_PRIVATE.store(true, Ordering::SeqCst);
        Guard
    }

    pub struct Guard;
    impl Drop for Guard {
        fn drop(&mut self) {
            ALLOW_PRIVATE.store(false, Ordering::SeqCst);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    #[test]
    fn classifies_addresses() {
        // Non-public (must be rejected).
        for ip in [
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)),
            IpAddr::V4(Ipv4Addr::new(172, 16, 5, 4)),
            IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1)),
            IpAddr::V4(Ipv4Addr::new(169, 254, 169, 254)), // cloud metadata
            IpAddr::V4(Ipv4Addr::new(100, 64, 0, 1)),      // CGNAT
            IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            IpAddr::V6(Ipv6Addr::LOCALHOST),
            IpAddr::V6("fc00::1".parse().unwrap()),
            IpAddr::V6("fe80::1".parse().unwrap()),
            IpAddr::V6("::ffff:127.0.0.1".parse().unwrap()), // mapped loopback
        ] {
            assert!(!is_public(ip), "{ip} should be non-public");
        }
        // Public (must be allowed).
        for ip in [
            IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1)),
            IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34)),
            IpAddr::V6("2606:4700:4700::1111".parse().unwrap()),
        ] {
            assert!(is_public(ip), "{ip} should be public");
        }
    }

    #[tokio::test]
    async fn rejects_non_http_scheme() {
        let url = Url::parse("ftp://example.com/x").unwrap();
        assert!(assert_public_url(&url).await.is_err());
    }

    #[tokio::test]
    async fn rejects_loopback_and_private_literals() {
        // IP literals resolve without DNS, so this is hermetic.
        for u in [
            "http://127.0.0.1/",
            "http://10.0.0.1/",
            "http://169.254.169.254/latest/meta-data/",
            "http://[::1]/",
            "http://[fc00::1]/",
        ] {
            let url = Url::parse(u).unwrap();
            assert!(
                assert_public_url(&url).await.is_err(),
                "{u} should be rejected"
            );
        }
    }
}
