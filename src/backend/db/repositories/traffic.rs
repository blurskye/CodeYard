//! Retained, pseudonymous observations. Raw client addresses/account IDs never enter SQL.
use anyhow::{ensure, Result};
use chrono::{NaiveDate, Utc};
use codeyard_db_core::native_operation;
use codeyard_db_core::DbPool;
use sha2::{Digest, Sha256};
use std::net::IpAddr;
#[cfg(test)]
mod tests;

#[derive(Clone, Copy)]
pub enum Kind {
    Views,
    Clones,
}
impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Views => "view",
            Self::Clones => "clone",
        }
    }
}
#[derive(Clone)]
pub struct Identity(String);
impl Identity {
    pub fn account(id: i64) -> Option<Self> {
        (id > 0).then(|| Self(format!("account:{id}")))
    }
    pub fn deploy_key(id: i64) -> Option<Self> {
        (id > 0).then(|| Self(format!("deploy:{id}")))
    }
    pub fn client(address: &str, agent: &str) -> Option<Self> {
        let address = address.parse::<IpAddr>().ok()?;
        let agent: String = agent.chars().take(512).collect();
        Some(Self(format!("client:{address}:agent:{agent}")))
    }
    pub fn fingerprint(&self, salt: &str) -> Result<String> {
        let key = hex::decode(salt)?;
        ensure!(key.len() == 32, "Invalid traffic privacy key");
        Ok(hex::encode(codeyard_support::crypto::hmac_sha256(
            &key,
            self.0.as_bytes(),
        )))
    }
}
#[derive(Clone)]
pub struct Metric {
    pub kind: Kind,
    pub path: String,
    pub path_hash: String,
    pub title: String,
    pub referrer: String,
}
impl Metric {
    pub fn view(path: &str, title: &str, referrer: Option<&str>, origin: &str) -> Result<Self> {
        let path = path.split('?').next().unwrap_or(path);
        ensure!(
            path.starts_with('/')
                && !path.starts_with("//")
                && path.len() <= 16384
                && !path.contains('\0'),
            "Invalid traffic path"
        );
        let origin = url::Url::parse(origin)?;
        let referrer = referrer
            .and_then(|value| url::Url::parse(value).ok())
            .filter(|url| {
                matches!(url.scheme(), "http" | "https")
                    && url.origin() != origin.origin()
                    && matches!(url.host(), Some(url::Host::Domain(_)))
            })
            .and_then(|url| url.host_str().map(str::to_owned))
            .filter(|host| host.len() <= 253)
            .unwrap_or_default();
        Ok(Self {
            kind: Kind::Views,
            path: path.to_owned(),
            path_hash: hex::encode(Sha256::digest(path.as_bytes())),
            title: title
                .chars()
                .filter(|c| !c.is_control())
                .take(512)
                .collect(),
            referrer,
        })
    }
    pub fn clone_transfer() -> Self {
        Self {
            kind: Kind::Clones,
            path: String::new(),
            path_hash: hex::encode(Sha256::digest([])),
            title: String::new(),
            referrer: String::new(),
        }
    }
}
#[derive(Clone)]
pub struct Window {
    pub today: String,
    pub yesterday: String,
    pub first: String,
}
impl Window {
    pub fn current() -> Self {
        Self::at(Utc::now().date_naive())
    }
    pub fn at(date: NaiveDate) -> Self {
        Self {
            today: date.to_string(),
            yesterday: (date - chrono::Duration::days(1)).to_string(),
            first: (date - chrono::Duration::days(13)).to_string(),
        }
    }
}
#[derive(sqlx::FromRow)]
pub(crate) struct DayKey {
    pub day: String,
    pub salt: String,
}
#[derive(serde::Serialize)]
pub(crate) struct Fingerprint {
    pub day: String,
    pub hash: String,
}
pub(crate) fn fingerprints(keys: &[DayKey], identity: &Identity) -> Result<Vec<Fingerprint>> {
    keys.iter()
        .map(|key| {
            Ok(Fingerprint {
                day: key.day.clone(),
                hash: identity.fingerprint(&key.salt)?,
            })
        })
        .collect()
}

pub(crate) fn new_salt() -> String {
    codeyard_support::crypto::random_hex()
}
#[derive(Clone, Debug)]
pub struct Summary {
    pub totals: Totals,
    pub buckets: Vec<Bucket>,
}
#[derive(Clone, Debug, sqlx::FromRow)]
pub struct Totals {
    pub count: i64,
    pub uniques: i64,
}
#[derive(Clone, Debug, sqlx::FromRow)]
pub struct Bucket {
    pub timestamp: String,
    pub count: i64,
    pub uniques: i64,
}
#[derive(Clone, Debug, sqlx::FromRow)]
pub struct PopularPath {
    pub path: String,
    pub title: String,
    pub count: i64,
    pub uniques: i64,
}
#[derive(Clone, Debug, sqlx::FromRow)]
pub struct PopularReferrer {
    pub referrer: String,
    pub count: i64,
    pub uniques: i64,
}

native_operation!(crate::backend::db::postgres::repositories::traffic::prune,
    crate::backend::db::sqlite::repositories::traffic::prune,
    fn prune(pool:&DbPool,first:&str)->Result<u64>);

native_operation!(crate::backend::db::postgres::repositories::traffic::record,
    crate::backend::db::sqlite::repositories::traffic::record,
    fn record(pool:&DbPool,repo:i64,identity:&Identity,metric:&Metric)->Result<()>);
native_operation!(crate::backend::db::postgres::repositories::traffic::summary,
    crate::backend::db::sqlite::repositories::traffic::summary,
    fn summary(pool:&DbPool,repo:i64,kind:Kind,window:&Window,weekly:bool)->Result<Summary>);
native_operation!(crate::backend::db::postgres::repositories::traffic::paths,
    crate::backend::db::sqlite::repositories::traffic::paths,
    fn paths(pool:&DbPool,repo:i64,window:&Window)->Result<Vec<PopularPath>>);
native_operation!(crate::backend::db::postgres::repositories::traffic::referrers,
    crate::backend::db::sqlite::repositories::traffic::referrers,
    fn referrers(pool:&DbPool,repo:i64,window:&Window)->Result<Vec<PopularReferrer>>);
