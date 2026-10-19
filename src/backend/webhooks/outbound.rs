//! Where a webhook may send. Private, loopback, link-local and metadata addresses are refused
//! unless `WEBHOOKS_ALLOW_PRIVATE=true`. The addresses are resolved once per attempt and the
//! request is pinned to them, so a name cannot change to a private address between the check
//! and the connection.
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use url::{Host, Url};

pub const MAX_URL_BYTES: usize = 2048;
const ALLOW_PRIVATE_ENV: &str = "WEBHOOKS_ALLOW_PRIVATE";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// The URL is not one a webhook may use. The text is safe to show.
    Invalid(&'static str),
    /// The name resolves only to addresses a webhook may not reach.
    Blocked,
    /// The name does not resolve, or no address answered in time.
    Unreachable,
}

pub fn allow_private() -> bool {
    std::env::var(ALLOW_PRIVATE_ENV).is_ok_and(|value| value == "true")
}

/// Checks the URL text a form saves. Resolution happens at delivery time.
pub fn validate_url(text: &str, allow_private: bool) -> Result<Url, Refusal> {
    if text.len() > MAX_URL_BYTES {
        return Err(Refusal::Invalid("Payload URL is too long"));
    }
    let url = Url::parse(text).map_err(|_| Refusal::Invalid("Payload URL must be a valid URL"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(Refusal::Invalid("Payload URL must use http or https"));
    }
    if url.host().is_none() {
        return Err(Refusal::Invalid("Payload URL needs a host"));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(Refusal::Invalid(
            "Payload URL cannot contain a username or password",
        ));
    }
    match url.host() {
        Some(Host::Ipv4(ip)) => check_addresses(&[IpAddr::V4(ip)], allow_private)?,
        Some(Host::Ipv6(ip)) => check_addresses(&[IpAddr::V6(ip)], allow_private)?,
        _ => {}
    }
    Ok(url)
}

/// Resolves the URL's host and returns every address the request may use.
pub async fn resolve(url: &Url, allow_private: bool) -> Result<Vec<SocketAddr>, Refusal> {
    let port = url
        .port_or_known_default()
        .ok_or(Refusal::Invalid("Payload URL needs a port"))?;
    let addresses: Vec<SocketAddr> = match url.host() {
        Some(Host::Domain(name)) => tokio::net::lookup_host((name, port))
            .await
            .map_err(|_| Refusal::Unreachable)?
            .collect(),
        Some(Host::Ipv4(ip)) => vec![SocketAddr::new(IpAddr::V4(ip), port)],
        Some(Host::Ipv6(ip)) => vec![SocketAddr::new(IpAddr::V6(ip), port)],
        None => return Err(Refusal::Invalid("Payload URL needs a host")),
    };
    if addresses.is_empty() {
        return Err(Refusal::Unreachable);
    }
    let ips: Vec<IpAddr> = addresses.iter().map(SocketAddr::ip).collect();
    check_addresses(&ips, allow_private)?;
    Ok(addresses)
}

/// Refuses the whole set when any address is private, so a name with one public and one
/// private address is not used either.
pub fn check_addresses(ips: &[IpAddr], allow_private: bool) -> Result<(), Refusal> {
    if allow_private {
        return Ok(());
    }
    if ips.iter().any(|ip| is_private(*ip)) {
        return Err(Refusal::Blocked);
    }
    Ok(())
}

pub fn is_private(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => is_private_v4(ip),
        IpAddr::V6(ip) => {
            if let Some(mapped) = ip.to_ipv4_mapped() {
                return is_private_v4(mapped);
            }
            is_private_v6(ip)
        }
    }
}

fn is_private_v4(ip: Ipv4Addr) -> bool {
    let [a, b, _, _] = ip.octets();
    ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_multicast()
        // Shared address space (carrier-grade NAT).
        || (a == 100 && (64..128).contains(&b))
}

fn is_private_v6(ip: Ipv6Addr) -> bool {
    let first = ip.segments()[0];
    ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_multicast()
        // Unique local (fc00::/7) and link-local (fe80::/10).
        || (first & 0xfe00) == 0xfc00
        || (first & 0xffc0) == 0xfe80
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_and_special_addresses_are_refused() {
        for text in [
            "http://127.0.0.1/hook",
            "http://10.1.2.3/hook",
            "http://192.168.0.5/hook",
            "http://172.16.9.9/hook",
            "http://169.254.169.254/latest/meta-data",
            "http://100.64.0.1/hook",
            "http://0.0.0.0/hook",
            "http://[::1]/hook",
            "http://[fd00::1]/hook",
            "http://[fe80::1]/hook",
            "http://[::ffff:10.0.0.1]/hook",
        ] {
            assert_eq!(
                validate_url(text, false).err(),
                Some(Refusal::Blocked),
                "{text} should be refused"
            );
        }
        assert!(validate_url("http://127.0.0.1/hook", true).is_ok());
    }

    #[test]
    fn public_addresses_and_names_are_allowed_at_save_time() {
        assert!(validate_url("https://93.184.216.34/hook", false).is_ok());
        assert!(validate_url("https://example.com/hook?x=1", false).is_ok());
    }

    #[test]
    fn schemes_credentials_and_length_are_refused() {
        assert!(matches!(
            validate_url("ftp://example.com/x", false),
            Err(Refusal::Invalid(_))
        ));
        assert!(matches!(
            validate_url("https://user:pw@example.com/x", false),
            Err(Refusal::Invalid(_))
        ));
        let long = format!("https://example.com/{}", "a".repeat(MAX_URL_BYTES));
        assert!(matches!(
            validate_url(&long, false),
            Err(Refusal::Invalid(_))
        ));
    }

    #[test]
    fn one_private_address_refuses_the_whole_set() {
        let mixed = [
            IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34)),
            IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)),
        ];
        assert_eq!(check_addresses(&mixed, false), Err(Refusal::Blocked));
        assert_eq!(check_addresses(&mixed, true), Ok(()));
    }
}
