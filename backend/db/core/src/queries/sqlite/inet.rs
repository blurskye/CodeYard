use anyhow::bail;
use anyhow::Result;
use std::net::IpAddr;
pub fn encode(value: Option<&str>) -> Result<Option<String>> {
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

pub fn address(value: IpAddr) -> String {
    format!("{value}/{}", if value.is_ipv4() { 32 } else { 128 })
}
