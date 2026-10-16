//! SQLite INET storage uses canonical address/prefix text, matching PostgreSQL
//! INET::text without weakening IPv4/IPv6 input validation.
use anyhow::{bail, Result};
use std::net::IpAddr;

pub(super) fn encode(value: Option<&str>) -> Result<Option<String>> {
    value
        .map(|value| {
            let (address, prefix) = value
                .split_once('/')
                .map_or((value, None), |(address, prefix)| (address, Some(prefix)));
            let address: IpAddr = address.parse()?;
            let maximum = if address.is_ipv4() { 32 } else { 128 };
            let prefix: u8 = match prefix {
                Some(prefix) => prefix.parse()?,
                None => maximum,
            };
            if prefix > maximum {
                bail!("Invalid network address prefix");
            }
            Ok(format!("{address}/{prefix}"))
        })
        .transpose()
}

pub(super) fn address(value: IpAddr) -> String {
    format!("{value}/{}", if value.is_ipv4() { 32 } else { 128 })
}
