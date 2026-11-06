//! Home-cell membership and star pointers discover remote resources; they never grant access.
use anyhow::{ensure, Result};
use codeyard_model::cells::{Object, ObjectKind};
use codeyard_support::cells::Prefix;
use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Membership,
    Star,
}
impl Kind {
    pub fn as_str(self) -> &'static str {
        match self { Self::Membership => "membership", Self::Star => "star" }
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Change {
    pub kind: Kind,
    pub user_id: i64,
    pub object: Object,
    pub active: bool,
}
impl Change {
    pub fn validate(&self) -> Result<()> {
        Prefix::for_id(self.user_id)?;
        self.object.validate()?;
        ensure!(self.kind != Kind::Star || self.object.kind == ObjectKind::Repository, "Invalid starred resource");
        ensure!(
            matches!(
                self.object.kind,
                ObjectKind::Organization | ObjectKind::Repository
            ),
            "Invalid resource resource"
        );
        Ok(())
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Event {
    pub id: i64,
    pub change: Change,
}
impl Event {
    pub fn validate(&self) -> Result<()> {
        Prefix::for_id(self.id)?;
        self.change.validate()
    }
}

/// The RPC boundary verifies the live cell key and both directory homes again at commit.
pub trait Admission: Send + Sync {
    fn check<'a>(&'a self, event: &'a Event) -> BoxFuture<'a, Result<Prefix>>;
}

pub const DEFAULT_PAGE_SIZE: usize = 100;

#[derive(Clone, Copy)]
pub struct Config {
    pub maximum_cells: usize,
    pub maximum_scan: usize,
    pub maximum_bytes: usize,
    pub page_size: usize,
    pub browser_repository_page_size: usize,
    pub request_seconds: u64,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            maximum_cells: 64,
            maximum_scan: 10000,
            maximum_bytes: 32 * 1024 * 1024,
            page_size: DEFAULT_PAGE_SIZE,
            browser_repository_page_size: 30,
            request_seconds: 10,
        }
    }
}
impl Config {
    pub fn validate(self) -> Result<()> {
        ensure!(
            (1..=256).contains(&self.maximum_cells)
                && (1..=100000).contains(&self.maximum_scan)
                && (65536..=256 * 1024 * 1024).contains(&self.maximum_bytes)
                && (1..=100).contains(&self.page_size)
                && (1..=100).contains(&self.browser_repository_page_size)
                && (1..=60).contains(&self.request_seconds),
            "Invalid remote resource limits"
        );
        Ok(())
    }
}

/// One row/byte budget is shared by cell reads and their merged presentation.
pub struct Budget {
    config: Config,
    count: usize,
    bytes: usize,
}
impl Budget {
    pub fn new(config: Config) -> Result<Self> {
        config.validate()?;
        Ok(Self {
            config,
            count: 0,
            bytes: 0,
        })
    }
    pub fn add<T: Serialize>(&mut self, value: &T) -> Result<()> {
        self.extend(1, serde_json::to_vec(value)?.len())
    }
    pub fn merge(&mut self, other: &Self) -> Result<()> {
        self.extend(other.count, other.bytes)
    }
    fn extend(&mut self, count: usize, bytes: usize) -> Result<()> {
        let count = self
            .count
            .checked_add(count)
            .ok_or_else(|| anyhow::anyhow!("Resource response exceeds its limit"))?;
        let bytes = self
            .bytes
            .checked_add(bytes)
            .ok_or_else(|| anyhow::anyhow!("Resource response exceeds its limit"))?;
        ensure!(
            count <= self.config.maximum_scan && bytes <= self.config.maximum_bytes,
            "Resource response exceeds its limit"
        );
        self.count = count;
        self.bytes = bytes;
        Ok(())
    }
}

async fn retain_postgres(connection: &mut sqlx::PgConnection, users: &[i64]) -> Result<()> {
    crate::queries::postgres::users::transitions::lock_accounts_shared(connection, users).await
}
async fn retain_sqlite(_connection: &mut sqlx::SqliteConnection, _users: &[i64]) -> Result<()> {
    // BEGIN IMMEDIATE retains the account through the pointer commit.
    Ok(())
}
macro_rules! engine {
    ($module:ident, $pool:ty, $begin:literal, $lock:literal, $retain:ident) => {
        pub mod $module {
            use super::*;
            pub struct Reader;
            pub async fn apply(pool: &$pool, event: &Event, admission: &dyn Admission) -> Result<()> {
                event.validate()?;
                let source = admission.check(event).await?;
                let mut tx = pool.begin_with($begin).await?;
                $retain(&mut tx, &[event.change.user_id]).await?;
                let user: Option<(bool, i32)> = sqlx::query_as(concat!("SELECT is_stub,home_cell FROM users WHERE id=$1", $lock))
                    .bind(event.change.user_id).fetch_optional(&mut *tx).await?;
                let Some((stub, home)) = user else { return Ok(()); };
                ensure!(!stub && home == i32::from(pool.cell_id().get()), "Pointer recipient has another home");
                ensure!(admission.check(event).await? == source, "Pointer source changed");
                let changed = sqlx::query("INSERT INTO remote_pointers(user_id,resource_kind,resource_id,source_cell,source_event,active,kind) VALUES($1,$2,$3,$4,$5,$6,$7) ON CONFLICT(user_id,kind,resource_kind,resource_id) DO UPDATE SET source_cell=excluded.source_cell,source_event=excluded.source_event,active=excluded.active WHERE remote_pointers.source_cell<>excluded.source_cell OR remote_pointers.source_event<excluded.source_event")
                    .bind(event.change.user_id).bind(event.change.object.kind.as_str()).bind(event.change.object.id)
                    .bind(i32::from(source.get())).bind(event.id).bind(event.change.active).bind(event.change.kind.as_str()).execute(&mut *tx).await?;
                if changed.rows_affected() == 0 {
                    let (version, active): (i64, bool) = sqlx::query_as("SELECT source_event,active FROM remote_pointers WHERE user_id=$1 AND resource_kind=$2 AND resource_id=$3 AND kind=$4")
                        .bind(event.change.user_id).bind(event.change.object.kind.as_str()).bind(event.change.object.id).bind(event.change.kind.as_str()).fetch_one(&mut *tx).await?;
                    ensure!(version != event.id || active == event.change.active, "Pointer event conflicts with its prior delivery");
                }
                ensure!(admission.check(event).await? == source, "Pointer source changed");
                tx.commit().await?;
                Ok(())
            }
        }
    };
}
engine!(
    postgres,
    crate::pool::PgPool,
    "BEGIN",
    " FOR SHARE",
    retain_postgres
);
engine!(
    sqlite,
    crate::pool::SqlitePool,
    "BEGIN IMMEDIATE",
    "",
    retain_sqlite
);

pub async fn apply(pool: &crate::DbPool, event: &Event, admission: &dyn Admission) -> Result<()> {
    let config = Config::default();
    config.validate()?;
    tokio::time::timeout(
        std::time::Duration::from_secs(config.request_seconds * 3 + 1),
        async {
            match pool {
                crate::DbPool::Postgres(pool) => postgres::apply(pool, event, admission).await,
                crate::DbPool::Sqlite(pool) => sqlite::apply(pool, event, admission).await,
            }
        },
    )
    .await?
}

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum List {
    Organizations,
    GraphqlOrganizations,
    Repositories,
    BrowserRepositories,
    BrowserOrganizations,
    StarredRepositories,
    BrowserStars,
}
impl List {
    pub fn browser(self) -> bool {
        matches!(self, Self::BrowserRepositories | Self::BrowserOrganizations | Self::BrowserStars)
    }
    pub fn stars(self) -> bool { matches!(self, Self::StarredRepositories | Self::BrowserStars) }
    pub fn page_size(self, config: Config) -> usize {
        if matches!(self, Self::BrowserRepositories) {
            config.browser_repository_page_size
        } else {
            config.page_size
        }
    }
}

pub struct PageResult {
    pub rows: Vec<serde_json::Value>,
    pub total: Option<i64>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Page {
    pub list: List,
    pub query: Vec<(String, String)>,
    pub number: u32,
    pub size: u16,
}
impl Page {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.number > 0 && (1..=100).contains(&self.size) && self.query.len() <= 16,
            "Invalid resource page"
        );
        if self.list.stars() {
            ensure!(self.query.iter().filter(|(key, _)| key == "username").count() == 1, "Invalid star owner");
        }
        let mut keys = std::collections::HashSet::new();
        for (key, value) in &self.query {
            ensure!(keys.insert(key), "Duplicate resource query");
            ensure!(
                value.len() <= 4096
                    && !value.chars().any(char::is_control)
                    && match self.list {
                        List::Repositories => matches!(
                            key.as_str(),
                            "type" | "visibility" | "affiliation" | "sort" | "direction" | "since"
                        ),
                        List::BrowserRepositories => key == "q" && value.chars().count() <= 100,
                        List::BrowserOrganizations =>
                            key == "after"
                                && value.parse::<i64>().ok().is_some_and(|id| (0
                                    ..=codeyard_support::cells::MAX_ID)
                                    .contains(&id)),
                        List::StarredRepositories | List::BrowserStars => match key.as_str() {
                            "username" => codeyard_model::account_input::validate_namespace(value, 1).is_ok(),
                            "sort" => matches!(value.as_str(), "created" | "updated" | "stars" | "name"),
                            "direction" => matches!(value.as_str(), "asc" | "desc"),
                            "q" => value.len() <= 256,
                            _ => false,
                        },
                        _ => false,
                    },
                "Invalid resource query"
            );
        }
        Ok(())
    }
}

macro_rules! read_engine {
    ($module:ident, $pool:ty, $begin:literal, $lock:literal, $retain:ident) => {
        impl $module::Reader {
            pub async fn homes(pool: &$pool, owner: i64, actor: Option<&crate::queries::credentials::Actor>, kind: Kind, config: Config) -> Result<Vec<Prefix>> {
                let mut tx = pool.begin_with($begin).await?;
                let mut users = vec![owner];
                if let Some(actor) = actor { users.push(actor.user_id); }
                $retain(&mut tx,&users).await?;
                let native: Option<i64> = sqlx::query_scalar(concat!("SELECT id FROM users WHERE id=$1 AND NOT is_stub AND home_cell=$2 AND NOT is_banned AND NOT must_change_password",$lock))
                    .bind(owner).bind(i32::from(pool.cell_id().get())).fetch_optional(&mut *tx).await?;
                ensure!(native.is_some(),codeyard_support::credentials::AccountRestricted);
                if let Some(actor) = actor {
                    let credential = actor.credential().ok_or(codeyard_support::credentials::AccountRestricted)?;
                    ensure!(crate::queries::$module::users::transitions::credential_live(&mut tx,actor.user_id,&credential,false).await?, codeyard_support::credentials::AccountRestricted);
                }
                let cells: Vec<i32> = sqlx::query_scalar("SELECT DISTINCT source_cell FROM remote_pointers WHERE user_id=$1 AND active AND source_cell<>$3 AND kind=$4 ORDER BY source_cell LIMIT $2")
                    .bind(owner).bind(config.maximum_cells as i64 + 1).bind(i32::from(pool.cell_id().get())).bind(kind.as_str()).fetch_all(&mut *tx).await?;
                ensure!(cells.len() <= config.maximum_cells,"Remote resource fanout exceeds its limit");
                if let Some(actor) = actor {
                    let credential = actor.credential().ok_or(codeyard_support::credentials::AccountRestricted)?;
                    ensure!(crate::queries::$module::users::transitions::credential_live(&mut tx,actor.user_id,&credential,false).await?, codeyard_support::credentials::AccountRestricted);
                }
                tx.commit().await?;
                cells.into_iter().map(|cell| Prefix::try_from(i64::from(cell)).map_err(Into::into)).collect()
            }
        }
    };
}
read_engine!(
    postgres,
    crate::pool::PgPool,
    "BEGIN",
    " FOR SHARE",
    retain_postgres
);
read_engine!(
    sqlite,
    crate::pool::SqlitePool,
    "BEGIN IMMEDIATE",
    "",
    retain_sqlite
);

pub async fn homes(pool: &crate::DbPool, actor: &crate::queries::credentials::Actor, config: Config) -> Result<Vec<Prefix>> {
    read_homes(pool, actor.user_id, Some(actor), Kind::Membership, config).await
}
pub async fn star_homes(pool: &crate::DbPool, owner: i64, viewer: Option<&crate::queries::credentials::Actor>, config: Config) -> Result<Vec<Prefix>> {
    read_homes(pool, owner, viewer, Kind::Star, config).await
}
async fn read_homes(pool: &crate::DbPool, owner: i64, actor: Option<&crate::queries::credentials::Actor>, kind: Kind, config: Config) -> Result<Vec<Prefix>> {
    config.validate()?;
    if pool.foreign_authorization().is_none() { return Ok(Vec::new()); }
    tokio::time::timeout(std::time::Duration::from_secs(config.request_seconds), async {
        match pool {
            crate::DbPool::Postgres(pool) => postgres::Reader::homes(pool, owner, actor, kind, config).await,
            crate::DbPool::Sqlite(pool) => sqlite::Reader::homes(pool, owner, actor, kind, config).await,
        }
    }).await?
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn membership_budget_rejects_large_rows_counts_and_merges_without_advancing() {
        let config = Config {
            maximum_scan: 2,
            maximum_bytes: 65536,
            ..Default::default()
        };
        let mut budget = Budget::new(config).unwrap();
        budget.add(&"first").unwrap();
        assert!(budget.add(&"x".repeat(65536)).is_err());
        budget.add(&true).unwrap();
        assert!(budget.add(&false).is_err());
        let mut other = Budget::new(config).unwrap();
        other.add(&true).unwrap();
        assert!(budget.merge(&other).is_err());
        assert_eq!(budget.count, 2);
    }
    #[test]
    fn browser_resource_pages_reject_hostile_and_unrelated_queries() {
        for (list, key, value) in [
            (List::BrowserRepositories, "q", "x".repeat(101)),
            (List::BrowserRepositories, "q", "a\rb".into()),
            (List::BrowserRepositories, "type", "all".into()),
            (List::BrowserOrganizations, "after", "-1".into()),
            (List::BrowserOrganizations, "after", i64::MAX.to_string()),
            (List::BrowserOrganizations, "q", "secret".into()),
        ] {
            assert!(Page {
                list,
                number: 1,
                size: 30,
                query: vec![(key.into(), value)]
            }
            .validate()
            .is_err());
        }
    }
}
