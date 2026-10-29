//! Future-publication policy and permanent immutable-tag reservations.
use crate::backend::{
    db::{self, DbPool},
    repositories::releases::integrity::{Artifact, Attestation, Statement},
};
use anyhow::{ensure, Result};

#[derive(Debug, thiserror::Error)]
pub enum Refusal {
    #[error("Repository administration required")]
    Denied,
    #[error("Archived repositories cannot change immutable release settings")]
    Archived,
}
/// Verified on the primary before it is passed into the release SQL transaction.
/// This type intentionally cannot be deserialized from an RPC/user request.
pub struct Publication {
    pub(crate) statement: Statement,
    pub(crate) attestation: String,
    pub(crate) signer_key: String,
}
impl Publication {
    pub fn verify(attestation: &Attestation, trusted_key: &str) -> Result<Self> {
        let statement = attestation.verify(trusted_key)?;
        Ok(Self {
            statement,
            attestation: serde_json::to_string(attestation)?,
            signer_key: trusted_key.into(),
        })
    }
    pub(crate) fn matches(&self, repo: i64, tag: &str, assets: &[Artifact], namespace: &str) -> Result<()> {
        let predicate = &self.statement.predicate;
        ensure!(
            predicate.repository_id == repo && predicate.tag == tag && predicate.assets == assets && predicate.repository == namespace,
            db::releases::ReleaseRefusal::Conflict
        );
        Ok(())
    }
}
#[derive(Debug, sqlx::FromRow)]
pub struct Reservation {
    pub repo_id: i64,
    pub tag_name: String,
    pub tag_oid: String,
    pub release_id: Option<i64>,
}
pub async fn enabled(pool: &DbPool, repo: i64) -> Result<bool> {
    match pool {
        DbPool::Sqlite(p) => db::sqlite::repositories::immutable_releases::enabled(p, repo).await,
        DbPool::Postgres(p) => {
            db::postgres::repositories::immutable_releases::enabled(p, repo).await
        }
    }
}
pub async fn set_enabled(pool: &DbPool, actor: i64, repo: i64, enabled: bool) -> Result<()> {
    match pool {
        DbPool::Sqlite(p) => {
            db::sqlite::repositories::immutable_releases::set_enabled(p, actor, repo, enabled).await
        }
        DbPool::Postgres(p) => {
            db::postgres::repositories::immutable_releases::set_enabled(p, actor, repo, enabled)
                .await
        }
    }
}
pub async fn reservations(
    pool: &DbPool,
    repo: i64,
    owner: &str,
    name: &str,
    tags: &[String],
) -> Result<Vec<Reservation>> {
    ensure!(tags.len() <= 10_000, "Too many tag references to check");
    if tags.is_empty() {
        return Ok(vec![]);
    }
    let rows = match pool {
        DbPool::Sqlite(p) => {
            db::sqlite::repositories::immutable_releases::reservations(p, repo, owner, name, tags)
                .await?
        }
        DbPool::Postgres(p) => {
            db::postgres::repositories::immutable_releases::reservations(p, repo, owner, name, tags)
                .await?
        }
    };
    ensure!(
        rows.len() <= 10_000,
        "Too much immutable tag history to check safely"
    );
    Ok(rows)
}
pub async fn assets(pool: &DbPool, release: i64) -> Result<Vec<Artifact>> {
    let mut assets = match pool {
        DbPool::Sqlite(p) => {
            db::sqlite::repositories::immutable_releases::assets(p, release).await?
        }
        DbPool::Postgres(p) => {
            db::postgres::repositories::immutable_releases::assets(p, release).await?
        }
    };
    ensure!(
        assets.len() as i64 <= db::releases::MAX_RELEASE_ASSETS,
        db::releases::ReleaseRefusal::TooManyAssets
    );
    assets.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(assets)
}
pub async fn attestation(
    pool: &DbPool,
    repo: i64,
    release: i64,
) -> Result<Option<(String, String)>> {
    match pool {
        DbPool::Sqlite(p) => {
            db::sqlite::repositories::immutable_releases::attestation(p, repo, release).await
        }
        DbPool::Postgres(p) => {
            db::postgres::repositories::immutable_releases::attestation(p, repo, release).await
        }
    }
}
