//! Opt-in download of remote email images ("Load remote images for this message").
//!
//! Hardened against abuse by hostile messages: HTTPS only, no cookies or referrer, short timeout,
//! response size cap, image content types only, and no connections to loopback, private or
//! link-local addresses (checked on IP literals, on DNS results and on every redirect hop).
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use url::{Host, Url};

pub const MAX_REMOTE_IMAGE_BYTES: usize = 5 * 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(10);
const MAX_REDIRECTS: usize = 3;

fn is_public_v4(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    !(ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_broadcast()
        || ip.is_multicast()
        || ip.is_documentation()
        || (o[0] == 100 && (64..128).contains(&o[1])) // CGNAT 100.64.0.0/10
        || o[0] == 0
        || o[0] >= 240)
}

fn is_public_v6(ip: Ipv6Addr) -> bool {
    if let Some(v4) = ip.to_ipv4_mapped() {
        return is_public_v4(v4);
    }
    let seg = ip.segments();
    !(ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_multicast()
        || (seg[0] & 0xfe00) == 0xfc00 // unique local fc00::/7
        || (seg[0] & 0xffc0) == 0xfe80) // link-local fe80::/10
}

pub fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_public_v4(v4),
        IpAddr::V6(v6) => is_public_v6(v6),
    }
}

/// Checks the scheme and host of a URL before any connection is attempted.
fn validate_url(url: &Url) -> Result<()> {
    if url.scheme() != "https" {
        bail!("only https images are loaded");
    }
    if !url.username().is_empty() || url.password().is_some() {
        bail!("URLs with credentials are refused");
    }
    match url.host() {
        Some(Host::Ipv4(ip)) if !is_public_v4(ip) => bail!("private address refused"),
        Some(Host::Ipv6(ip)) if !is_public_v6(ip) => bail!("private address refused"),
        Some(Host::Domain(d)) => {
            let d = d.to_ascii_lowercase();
            if d == "localhost" || d.ends_with(".localhost") || d.ends_with(".local") || d.ends_with(".internal") {
                bail!("local host name refused");
            }
        }
        Some(_) => {}
        None => bail!("URL has no host"),
    }
    Ok(())
}

/// Resolves names via the system resolver but drops non-public addresses (DNS rebinding defence).
struct PublicOnlyResolver;

impl Resolve for PublicOnlyResolver {
    fn resolve(&self, name: Name) -> Resolving {
        Box::pin(async move {
            let found = tokio::net::lookup_host((name.as_str(), 0)).await?;
            let public: Vec<SocketAddr> = found.filter(|a| is_public_ip(a.ip())).collect();
            if public.is_empty() {
                let err: Box<dyn std::error::Error + Send + Sync> =
                    "host resolves only to non-public addresses".into();
                return Err(err);
            }
            Ok(Box::new(public.into_iter()) as Addrs)
        })
    }
}

fn client() -> Result<reqwest::Client> {
    let redirects = reqwest::redirect::Policy::custom(|attempt| {
        if attempt.previous().len() >= MAX_REDIRECTS || validate_url(attempt.url()).is_err() {
            attempt.stop()
        } else {
            attempt.follow()
        }
    });
    Ok(reqwest::Client::builder()
        .dns_resolver(std::sync::Arc::new(PublicOnlyResolver))
        .redirect(redirects)
        .timeout(TIMEOUT)
        .referer(false)
        .user_agent("Mozilla/5.0")
        .build()?)
}

/// Downloads one remote image. Returns an error for anything that is not a small image over HTTPS.
pub async fn fetch_remote_image(url: &str) -> Result<Vec<u8>> {
    let parsed = Url::parse(url).map_err(|_| anyhow!("invalid image URL"))?;
    validate_url(&parsed)?;

    let mut resp = client()?.get(parsed).send().await?;
    if !resp.status().is_success() {
        bail!("image server returned {}", resp.status());
    }
    let is_image = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.trim().to_ascii_lowercase().starts_with("image/"));
    if !is_image {
        bail!("response is not an image");
    }
    if resp.content_length().is_some_and(|n| n as usize > MAX_REMOTE_IMAGE_BYTES) {
        bail!("image is too large");
    }

    let mut data = Vec::new();
    while let Some(chunk) = resp.chunk().await? {
        if data.len() + chunk.len() > MAX_REMOTE_IMAGE_BYTES {
            bail!("image is too large");
        }
        data.extend_from_slice(&chunk);
    }
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn rejects_non_https_credentials_and_local_hosts() {
        assert!(validate_url(&url("https://cdn.example.com/a.png")).is_ok());
        assert!(validate_url(&url("http://cdn.example.com/a.png")).is_err());
        assert!(validate_url(&url("https://user:pw@cdn.example.com/a.png")).is_err());
        assert!(validate_url(&url("https://localhost/a.png")).is_err());
        assert!(validate_url(&url("https://printer.local/a.png")).is_err());
        assert!(validate_url(&url("https://127.0.0.1/a.png")).is_err());
        assert!(validate_url(&url("https://192.168.1.10/a.png")).is_err());
        assert!(validate_url(&url("https://169.254.169.254/latest/meta-data")).is_err());
        assert!(validate_url(&url("https://[::1]/a.png")).is_err());
        assert!(validate_url(&url("https://[::ffff:10.0.0.1]/a.png")).is_err());
        assert!(validate_url(&url("https://93.184.216.34/a.png")).is_ok());
    }

    #[test]
    fn classifies_ip_ranges() {
        for ip in ["10.1.2.3", "172.16.0.1", "100.64.0.1", "0.0.0.0", "224.0.0.1", "fc00::1", "fe80::1", "::"] {
            assert!(!is_public_ip(ip.parse().unwrap()), "{ip} should be non-public");
        }
        for ip in ["8.8.8.8", "1.1.1.1", "2606:4700:4700::1111"] {
            assert!(is_public_ip(ip.parse().unwrap()), "{ip} should be public");
        }
    }

    #[tokio::test]
    async fn fetch_refuses_private_targets_without_connecting() {
        assert!(fetch_remote_image("https://127.0.0.1:1/x.png").await.is_err());
        assert!(fetch_remote_image("file:///etc/passwd").await.is_err());
        assert!(fetch_remote_image("not a url").await.is_err());
    }
}
