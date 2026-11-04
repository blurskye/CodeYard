//! Release content and draft visibility share one SQLite read transaction.
use super::super::repositories::access::USER_REPO_ACCESS_SQL;
use super::{ASSET_COLUMNS, ROW_SELECT};
use crate::backend::db::releases::ReleaseRefusal;
use crate::backend::db::releases::ReleaseSelection;
use crate::backend::db::releases::ReleaseSnapshot;
use anyhow::Result;
use codeyard_db_core::models::ReleaseAsset;
use codeyard_db_core::models::ReleaseRow;
use codeyard_db_core::models::Repository;
use sqlx::SqlitePool;

pub async fn read_release_snapshot(
    pool: &SqlitePool,
    viewer: Option<i64>,
    repo_id: i64,
    selection: ReleaseSelection<'_>,
) -> Result<ReleaseSnapshot> {
    let mut tx = pool.begin().await?;
    let repository = sqlx::query_as::<_, Repository>(&format!(
        "SELECT r.* FROM repositories r LEFT JOIN users u ON u.id=?1 AND NOT u.is_banned AND NOT u.must_change_password WHERE r.id=?2 AND NOT r.is_gist AND (NOT r.is_private OR (r.owner_kind='user' AND u.is_admin) OR ({USER_REPO_ACCESS_SQL})>=1)"
    )).bind(viewer).bind(repo_id).fetch_optional(&mut *tx).await?
        .ok_or(ReleaseRefusal::NotFound)?;
    let drafts: bool = sqlx::query_scalar(&format!(
        "SELECT COALESCE((r.owner_kind='user' AND u.is_admin) OR ({USER_REPO_ACCESS_SQL})>=3,0) FROM repositories r LEFT JOIN users u ON u.id=?1 AND NOT u.is_banned AND NOT u.must_change_password WHERE r.id=?2"
    )).bind(viewer).bind(repo_id).fetch_one(&mut *tx).await?;
    let (id, tag, limit, offset) = match selection {
        ReleaseSelection::Page { limit, offset } => {
            anyhow::ensure!(
                (1..=100).contains(&limit) && offset >= 0,
                ReleaseRefusal::NotFound
            );
            (None, None, limit, offset)
        }
        ReleaseSelection::Id(id) => (Some(id), None, 1, 0),
        ReleaseSelection::Tag(tag) => (None, Some(tag), 1, 0),
    };
    let releases = sqlx::query_as::<_, ReleaseRow>(&format!(
        "{ROW_SELECT} WHERE r.repo_id=?1 AND (?2 OR NOT r.is_draft) AND (?3 IS NULL OR r.id=?3) AND (?4 IS NULL OR r.tag_name=?4) ORDER BY r.created_at DESC, r.id DESC LIMIT ?5 OFFSET ?6"
    )).bind(repo_id).bind(drafts).bind(id).bind(tag).bind(limit).bind(offset)
        .fetch_all(&mut *tx).await?;
    let total = sqlx::query_scalar(
        "SELECT COUNT(*) FROM releases WHERE repo_id=?1 AND (?2 OR NOT is_draft)",
    )
    .bind(repo_id)
    .bind(drafts)
    .fetch_one(&mut *tx)
    .await?;
    let ids = releases
        .iter()
        .map(|row| row.release.id)
        .collect::<Vec<_>>();
    let assets = sqlx::query_as::<_, ReleaseAsset>(&format!(
        "SELECT {ASSET_COLUMNS} FROM release_assets WHERE release_id IN (SELECT value FROM json_each(?1)) ORDER BY id"
    )).bind(serde_json::to_string(&ids)?).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(ReleaseSnapshot {
        repository,
        releases,
        assets,
        total,
    })
}
