//! Directory records and validated keys shared by cell clients and routers.
use crate::account_input::{validate_email, validate_namespace};
use anyhow::{ensure, Result};
use base64::{engine::general_purpose::STANDARD_NO_PAD, Engine};
use codeyard_support::cells::Prefix;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const MAX_ENDPOINTS: usize = 1000;
pub const MAX_RECORD_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObjectKind {
    User,
    Organization,
    Repository,
}
impl ObjectKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Organization => "organization",
            Self::Repository => "repository",
        }
    }
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "user" => Some(Self::User),
            "organization" => Some(Self::Organization),
            "repository" => Some(Self::Repository),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Object {
    pub kind: ObjectKind,
    pub id: i64,
}
impl Object {
    pub fn validate(self) -> Result<()> {
        Prefix::for_id(self.id)?;
        Ok(())
    }
    /// The directory shard that holds this object's home row.
    pub fn shard(self) -> i16 {
        let mut hash = Sha256::new();
        hash.update(self.kind.as_str());
        hash.update([0]);
        hash.update(self.id.to_be_bytes());
        i16::from(hash.finalize()[0])
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyKind {
    Name,
    Email,
    Ssh,
    /// The hash of a token issued by another cell before its account moved.
    Credential,
}
impl KeyKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Name => "name",
            Self::Email => "email",
            Self::Ssh => "ssh",
            Self::Credential => "credential",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "KeyInput", into = "KeyInput")]
pub struct Key {
    kind: KeyKind,
    value: String,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct KeyInput {
    kind: KeyKind,
    value: String,
}
impl TryFrom<KeyInput> for Key {
    type Error = anyhow::Error;
    fn try_from(value: KeyInput) -> Result<Self> {
        Self::new(value.kind, &value.value)
    }
}
impl From<Key> for KeyInput {
    fn from(value: Key) -> Self {
        Self {
            kind: value.kind,
            value: value.value,
        }
    }
}
impl Key {
    pub fn new(kind: KeyKind, value: &str) -> Result<Self> {
        let value = match kind {
            KeyKind::Name => {
                let value = value.to_ascii_lowercase();
                ensure!(
                    value.len() <= 1024 && value.chars().count() <= 255,
                    "Invalid account name"
                );
                validate_namespace(value.strip_suffix("[bot]").unwrap_or(&value), 1)
                    .map_err(|_| anyhow::anyhow!("Invalid account name"))?;
                value
            }
            KeyKind::Email => {
                validate_email(value).map_err(|_| anyhow::anyhow!("Invalid email address"))?;
                value.to_ascii_lowercase()
            }
            KeyKind::Ssh => {
                let encoded = value
                    .strip_prefix("SHA256:")
                    .ok_or_else(|| anyhow::anyhow!("Invalid SSH fingerprint"))?;
                ensure!(encoded.len() == 43, "Invalid SSH fingerprint");
                let bytes = STANDARD_NO_PAD.decode(encoded)?;
                ensure!(
                    bytes.len() == 32 && STANDARD_NO_PAD.encode(bytes) == encoded,
                    "Invalid SSH fingerprint"
                );
                value.to_owned()
            }
            KeyKind::Credential => {
                ensure!(
                    codeyard_support::tokens::is_hex_token(value, 64),
                    "Invalid credential hash"
                );
                value.to_ascii_lowercase()
            }
        };
        Ok(Self { kind, value })
    }
    pub fn kind(&self) -> KeyKind {
        self.kind
    }
    pub fn value(&self) -> &str {
        &self.value
    }
    pub fn hash(&self) -> [u8; 32] {
        let mut hash = Sha256::new();
        hash.update(self.kind.as_str());
        hash.update([0]);
        hash.update(self.value.as_bytes());
        hash.finalize().into()
    }
    pub fn shard(&self) -> i16 {
        i16::from(self.hash()[0])
    }
    pub fn belongs_to(&self, object: ObjectKind) -> bool {
        if self.kind == KeyKind::Name && self.value.ends_with("[bot]") {
            return object == ObjectKind::User;
        }
        matches!(
            (self.kind, object),
            (KeyKind::Name, ObjectKind::User | ObjectKind::Organization)
                | (KeyKind::Email, ObjectKind::User)
                | (KeyKind::Ssh, ObjectKind::User | ObjectKind::Repository)
                | (KeyKind::Credential, ObjectKind::User)
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Open,
    Closed,
    Disabled,
}
impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Closed => "closed",
            Self::Disabled => "disabled",
        }
    }
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "open" => Some(Self::Open),
            "closed" => Some(Self::Closed),
            "disabled" => Some(Self::Disabled),
            _ => None,
        }
    }
}

/// Set by a router on every request it forwards to a cell, after removing any value the
/// client sent. Cells accept it only from a trusted proxy.
pub const REGION_HEADER: &str = "x-codeyard-region";

pub fn validate_region(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty()
            && value.len() <= 63
            && value
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-'),
        "Invalid region"
    );
    Ok(())
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cell {
    pub id: Prefix,
    pub name: String,
    pub region: String,
    pub state: State,
    pub web_urls: Vec<String>,
    pub ssh_addresses: Vec<String>,
    pub rpc_addresses: Vec<String>,
    pub node_keys: Vec<String>,
    pub database_bytes: i64,
    pub write_rate: i64,
    pub maximum_database_bytes: i64,
    pub maximum_write_rate: i64,
    /// Web nodes in other regions that serve this cell's reads from a local database replica
    /// and Git copies, for organizations with presence there. Writes always go to `web_urls`.
    /// Left out when empty, so routers that predate read regions still read the record.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub read_regions: Vec<ReadRegion>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadRegion {
    pub region: String,
    pub web_urls: Vec<String>,
}

pub const MAX_READ_REGIONS: usize = 16;

/// A cell web address: HTTPS, or HTTP on loopback, with no path, query or credentials.
fn web_url(address: &str) -> Result<String> {
    ensure!(address.len() <= 2048, "Cell URL too long");
    let url = url::Url::parse(address)?;
    let local = matches!(url.host(), Some(url::Host::Ipv4(ip)) if ip.is_loopback())
        || matches!(url.host(), Some(url::Host::Ipv6(ip)) if ip.is_loopback())
        || url.host_str() == Some("localhost");
    ensure!(
        (url.scheme() == "https" || (url.scheme() == "http" && local))
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.path() == "/"
            && url.query().is_none()
            && url.fragment().is_none(),
        "Invalid cell URL"
    );
    Ok(url.as_str().trim_end_matches('/').to_owned())
}

impl Cell {
    /// The web nodes that serve this cell's reads in `region`, when it has any there.
    pub fn read_urls(&self, region: &str) -> Option<&[String]> {
        self.read_regions
            .iter()
            .find(|read| read.region == region)
            .map(|read| read.web_urls.as_slice())
    }
    pub fn prepare(mut self) -> Result<Self> {
        ensure!(
            !self.name.is_empty()
                && self.name.len() <= 100
                && self
                    .name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte)),
            "Invalid cell name"
        );
        validate_region(&self.region)?;
        ensure!(
            !self.web_urls.is_empty()
                && self.web_urls.len() <= MAX_ENDPOINTS
                && self.ssh_addresses.len() <= MAX_ENDPOINTS
                && self.rpc_addresses.len() <= MAX_ENDPOINTS
                && !self.node_keys.is_empty()
                && self.node_keys.len() <= MAX_ENDPOINTS,
            "Invalid cell endpoints"
        );
        for address in &mut self.web_urls {
            *address = web_url(address)?;
        }
        ensure!(
            self.read_regions.len() <= MAX_READ_REGIONS,
            "Too many read regions"
        );
        for read in &mut self.read_regions {
            validate_region(&read.region)?;
            ensure!(
                read.region != self.region
                    && !read.web_urls.is_empty()
                    && read.web_urls.len() <= MAX_ENDPOINTS,
                "Invalid read region"
            );
            for address in &mut read.web_urls {
                *address = web_url(address)?;
            }
            read.web_urls.sort();
            read.web_urls.dedup();
        }
        self.read_regions.sort_by(|a, b| a.region.cmp(&b.region));
        let regions = self.read_regions.len();
        self.read_regions.dedup_by(|a, b| a.region == b.region);
        ensure!(
            self.read_regions.len() == regions,
            "A read region is listed twice"
        );
        for address in self
            .ssh_addresses
            .iter_mut()
            .chain(self.rpc_addresses.iter_mut())
        {
            *address = address.parse::<std::net::SocketAddr>()?.to_string();
        }
        for key in &mut self.node_keys {
            ensure!(
                codeyard_support::tokens::is_hex_token(key, 64),
                "Invalid cell node key"
            );
            *key = key.to_ascii_lowercase();
        }
        self.web_urls.sort();
        self.web_urls.dedup();
        self.ssh_addresses.sort();
        self.ssh_addresses.dedup();
        self.rpc_addresses.sort();
        self.rpc_addresses.dedup();
        self.node_keys.sort();
        self.node_keys.dedup();
        ensure!(
            self.database_bytes >= 0
                && self.write_rate >= 0
                && (1..=4_000_000_000_000).contains(&self.maximum_database_bytes)
                && (1..=10_000).contains(&self.maximum_write_rate),
            "Invalid cell capacity"
        );
        ensure!(
            serde_json::to_vec(&self)?.len() <= MAX_RECORD_BYTES,
            "Cell record too large"
        );
        Ok(self)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Location {
    pub cell: Prefix,
    pub object: Object,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cell_records_without_read_regions_keep_the_older_shape() {
        let older = serde_json::json!({
            "id": 0, "name": "zero", "region": "a", "state": "open",
            "web_urls": ["http://127.0.0.1:1"], "ssh_addresses": [], "rpc_addresses": [],
            "node_keys": ["a".repeat(64)], "database_bytes": 0, "write_rate": 0,
            "maximum_database_bytes": 10, "maximum_write_rate": 10
        });
        let cell: Cell = serde_json::from_value(older.clone()).unwrap();
        let cell = cell.prepare().unwrap();
        assert!(cell.read_regions.is_empty());
        assert_eq!(serde_json::to_value(&cell).unwrap(), older);
        let mut nearby = cell.clone();
        nearby.read_regions = vec![ReadRegion {
            region: "b".into(),
            web_urls: vec!["http://127.0.0.1:2/".into()],
        }];
        let nearby = nearby.prepare().unwrap();
        assert_eq!(
            nearby.read_urls("b"),
            Some(&["http://127.0.0.1:2".to_owned()][..])
        );
        assert_eq!(nearby.read_urls("a"), None);
        for hostile in [
            vec![ReadRegion {
                region: "a".into(),
                web_urls: vec!["http://127.0.0.1:2".into()],
            }],
            vec![ReadRegion {
                region: "b".into(),
                web_urls: vec![],
            }],
            vec![ReadRegion {
                region: "b".into(),
                web_urls: vec!["http://example.test".into()],
            }],
            vec![ReadRegion {
                region: "B".into(),
                web_urls: vec!["http://127.0.0.1:2".into()],
            }],
            vec![
                ReadRegion {
                    region: "b".into(),
                    web_urls: vec!["http://127.0.0.1:2".into()],
                },
                ReadRegion {
                    region: "b".into(),
                    web_urls: vec!["http://127.0.0.1:3".into()],
                },
            ],
        ] {
            let mut cell = cell.clone();
            cell.read_regions = hostile;
            assert!(cell.prepare().is_err());
        }
    }
    #[test]
    fn bot_names_share_the_directory_validator_and_only_name_users() {
        let bot = Key::new(KeyKind::Name, "Actions[BoT]").unwrap();
        assert_eq!(bot.value(), "actions[bot]");
        assert!(bot.belongs_to(ObjectKind::User));
        assert!(!bot.belongs_to(ObjectKind::Organization));
        assert!(Key::new(KeyKind::Name, "[bot]").is_err());
        assert!(Key::new(KeyKind::Name, &format!("{}[bot]", "a".repeat(251))).is_err());
    }
}
