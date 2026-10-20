use crate::backend::db::models::{File, Release, ReleaseAsset, ReleaseRow};
use crate::backend::db::releases::{
    NewReleaseAsset, ReleaseChanges, ReleaseFields, ReleaseRefusal,
};
use anyhow::Result;
use sqlx::{Sqlite, SqlitePool, Transaction};
use uuid::Uuid;

const RELEASE_COLUMNS: &str = "r.id, r.repo_id, r.tag_name, r.target_commitish, r.name, r.body, r.is_draft, r.is_prerelease, r.is_latest, r.author_id, r.created_at, r.published_at";

const ROW_SELECT: &str = "SELECT r.id, r.repo_id, r.tag_name, r.target_commitish, r.name, r.body, r.is_draft, r.is_prerelease, r.is_latest, r.author_id, r.created_at, r.published_at, u.username AS author FROM releases r JOIN users u ON u.id = r.author_id";

const ASSET_COLUMNS: &str =
    "id, release_id, file_id, name, label, content_type, size, download_count, created_at, updated_at";

/// Writes take the write lock first, so the checks and the change see the same state.
async fn begin_write(
    pool: &SqlitePool,
    actor_id: i64,
    repo_id: i64,
) -> Result<Transaction<'_, Sqlite>> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    anyhow::ensure!(
        super::discussions::lock_write_authority(&mut tx, actor_id, repo_id).await?,
        ReleaseRefusal::Denied
    );
    Ok(tx)
}

async fn release_in_repo(
    tx: &mut Transaction<'_, Sqlite>,
    repo_id: i64,
    release_id: i64,
) -> Result<Release> {
    sqlx::query_as::<_, Release>(&format!(
        "SELECT {RELEASE_COLUMNS} FROM releases r WHERE r.id = ?1 AND r.repo_id = ?2"
    ))
    .bind(release_id)
    .bind(repo_id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or_else(|| ReleaseRefusal::NotFound.into())
}

/// Clears the latest flag from the repository's other releases when `release_id` takes it.
/// A draft or a pre-release never holds the flag.
async fn set_latest_flag(
    tx: &mut Transaction<'_, Sqlite>,
    repo_id: i64,
    release_id: i64,
    fields: &ReleaseFields,
) -> Result<()> {
    let wants = fields.make_latest && !fields.is_draft && !fields.is_prerelease;
    if wants {
        sqlx::query(
            "UPDATE releases SET is_latest = 0 WHERE repo_id = ?1 AND is_latest = 1 AND id <> ?2",
        )
        .bind(repo_id)
        .bind(release_id)
        .execute(&mut **tx)
        .await?;
    }
    sqlx::query("UPDATE releases SET is_latest = ?1 WHERE id = ?2")
        .bind(wants)
        .bind(release_id)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

pub async fn create_release(
    pool: &SqlitePool,
    actor_id: i64,
    repo_id: i64,
    tag: &str,
    target: &str,
    fields: &ReleaseFields,
) -> Result<Release> {
    let mut tx = begin_write(pool, actor_id, repo_id).await?;
    let taken: Option<i64> =
        sqlx::query_scalar("SELECT id FROM releases WHERE repo_id = ?1 AND tag_name = ?2")
            .bind(repo_id)
            .bind(tag)
            .fetch_optional(&mut *tx)
            .await?;
    anyhow::ensure!(taken.is_none(), ReleaseRefusal::TagTaken);
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO releases (repo_id, tag_name, target_commitish, name, body, is_draft, is_prerelease, author_id, published_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8,
                 CASE WHEN ?6 = 0 THEN strftime('%Y-%m-%dT%H:%M:%f000Z','now') END)
         RETURNING id",
    )
    .bind(repo_id)
    .bind(tag)
    .bind(target)
    .bind(&fields.name)
    .bind(&fields.body)
    .bind(fields.is_draft)
    .bind(fields.is_prerelease)
    .bind(actor_id)
    .fetch_one(&mut *tx)
    .await?;
    set_latest_flag(&mut tx, repo_id, id, fields).await?;
    let release = release_in_repo(&mut tx, repo_id, id).await?;
    tx.commit().await?;
    Ok(release)
}

pub async fn update_release(
    pool: &SqlitePool,
    actor_id: i64,
    repo_id: i64,
    release_id: i64,
    fields: &ReleaseChanges,
) -> Result<(Release, bool)> {
    let mut tx = begin_write(pool, actor_id, repo_id).await?;
    let current = release_in_repo(&mut tx, repo_id, release_id).await?;
    if let Some(expected) = &fields.expected_revision {
        anyhow::ensure!(
            *expected == crate::backend::db::releases::release_revision(&current),
            ReleaseRefusal::Conflict
        );
    }
    let fields = fields.apply_to(&current);
    // A release first published keeps its date. A draft gets one when it is published.
    sqlx::query(
        "UPDATE releases SET name = ?1, body = ?2, is_draft = ?3, is_prerelease = ?4,
             published_at = CASE WHEN ?3 = 1 THEN NULL
                                 WHEN published_at IS NULL THEN strftime('%Y-%m-%dT%H:%M:%f000Z','now')
                                 ELSE published_at END
         WHERE id = ?5",
    )
    .bind(&fields.name)
    .bind(&fields.body)
    .bind(fields.is_draft)
    .bind(fields.is_prerelease)
    .bind(release_id)
    .execute(&mut *tx)
    .await?;
    set_latest_flag(&mut tx, repo_id, release_id, &fields).await?;
    let release = release_in_repo(&mut tx, repo_id, release_id).await?;
    let published = current.is_draft && !release.is_draft;
    tx.commit().await?;
    Ok((release, published))
}

pub async fn delete_release(
    pool: &SqlitePool,
    actor_id: i64,
    repo_id: i64,
    release_id: i64,
) -> Result<Vec<File>> {
    let mut tx = begin_write(pool, actor_id, repo_id).await?;
    release_in_repo(&mut tx, repo_id, release_id).await?;
    let files: Vec<Uuid> = sqlx::query_scalar(
        "SELECT DISTINCT file_id FROM release_assets WHERE release_id = ?1 ORDER BY file_id",
    )
    .bind(release_id)
    .fetch_all(&mut *tx)
    .await?;
    sqlx::query("DELETE FROM releases WHERE id = ?1")
        .bind(release_id)
        .execute(&mut *tx)
        .await?;
    let mut removed = Vec::new();
    for id in files {
        if let Some(file) = delete_asset_file(&mut tx, id).await? {
            removed.push(file);
        }
    }
    tx.commit().await?;
    Ok(removed)
}

pub async fn add_release_asset(
    pool: &SqlitePool,
    actor_id: i64,
    repo_id: i64,
    release_id: i64,
    asset: &NewReleaseAsset<'_>,
) -> Result<ReleaseAsset> {
    let mut tx = begin_write(pool, actor_id, repo_id).await?;
    release_in_repo(&mut tx, repo_id, release_id).await?;
    let taken: Option<i64> =
        sqlx::query_scalar("SELECT id FROM release_assets WHERE release_id = ?1 AND name = ?2")
            .bind(release_id)
            .bind(asset.name)
            .fetch_optional(&mut *tx)
            .await?;
    anyhow::ensure!(taken.is_none(), ReleaseRefusal::AssetNameTaken);
    let activated = sqlx::query(
        "UPDATE files SET is_active=TRUE, upload_expires_at=NULL
         WHERE id=?1 AND user_id=?2 AND reason='release_asset' AND NOT is_active
           AND confirmed_at IS NOT NULL AND upload_expires_at > strftime('%Y-%m-%dT%H:%M:%f000Z','now')",
    )
    .bind(asset.file_id)
    .bind(actor_id)
    .execute(&mut *tx)
    .await?;
    anyhow::ensure!(activated.rows_affected() == 1, ReleaseRefusal::NotFound);
    let asset = sqlx::query_as::<_, ReleaseAsset>(&format!(
        "INSERT INTO release_assets (release_id, file_id, name, label, content_type, size)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6) RETURNING {ASSET_COLUMNS}"
    ))
    .bind(release_id)
    .bind(asset.file_id)
    .bind(asset.name)
    .bind(asset.label)
    .bind(asset.content_type)
    .bind(asset.size)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(asset)
}

pub async fn delete_release_asset(
    pool: &SqlitePool,
    actor_id: i64,
    repo_id: i64,
    release_id: i64,
    asset_id: i64,
) -> Result<Option<File>> {
    let mut tx = begin_write(pool, actor_id, repo_id).await?;
    release_in_repo(&mut tx, repo_id, release_id).await?;
    let file: Option<Uuid> = sqlx::query_scalar(
        "DELETE FROM release_assets WHERE id = ?1 AND release_id = ?2 RETURNING file_id",
    )
    .bind(asset_id)
    .bind(release_id)
    .fetch_optional(&mut *tx)
    .await?;
    let file = file.ok_or(ReleaseRefusal::NotFound)?;
    let removed = delete_asset_file(&mut tx, file).await?;
    tx.commit().await?;
    Ok(removed)
}

async fn delete_asset_file(tx: &mut Transaction<'_, Sqlite>, id: Uuid) -> Result<Option<File>> {
    let file = sqlx::query_as::<_, File>(
        "SELECT * FROM files WHERE id=?1 AND reason='release_asset'
         AND NOT EXISTS(SELECT 1 FROM release_assets WHERE file_id=?1)",
    )
    .bind(id)
    .fetch_optional(&mut **tx)
    .await?;
    if let Some(file) = &file {
        super::files::queue_deletion(tx, file).await?;
        sqlx::query("DELETE FROM files WHERE id=?1")
            .bind(id)
            .execute(&mut **tx)
            .await?;
    }
    Ok(file)
}

pub async fn get_release_by_tag(
    pool: &SqlitePool,
    repo_id: i64,
    tag: &str,
) -> Result<Option<ReleaseRow>> {
    Ok(sqlx::query_as::<_, ReleaseRow>(&format!(
        "{ROW_SELECT} WHERE r.repo_id = ?1 AND r.tag_name = ?2"
    ))
    .bind(repo_id)
    .bind(tag)
    .fetch_optional(pool)
    .await?)
}

pub async fn get_release(
    pool: &SqlitePool,
    repo_id: i64,
    release_id: i64,
) -> Result<Option<ReleaseRow>> {
    Ok(
        sqlx::query_as::<_, ReleaseRow>(&format!(
            "{ROW_SELECT} WHERE r.repo_id = ?1 AND r.id = ?2"
        ))
        .bind(repo_id)
        .bind(release_id)
        .fetch_optional(pool)
        .await?,
    )
}

pub async fn list_releases(
    pool: &SqlitePool,
    repo_id: i64,
    include_drafts: bool,
    limit: i64,
    offset: i64,
) -> Result<Vec<ReleaseRow>> {
    Ok(sqlx::query_as::<_, ReleaseRow>(&format!(
        "{ROW_SELECT} WHERE r.repo_id = ?1 AND (?2 OR r.is_draft = 0)
         ORDER BY COALESCE(r.published_at, r.created_at) DESC, r.id DESC LIMIT ?3 OFFSET ?4"
    ))
    .bind(repo_id)
    .bind(include_drafts)
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await?)
}

pub async fn count_releases(pool: &SqlitePool, repo_id: i64, include_drafts: bool) -> Result<i64> {
    Ok(sqlx::query_scalar(
        "SELECT COUNT(*) FROM releases WHERE repo_id = ?1 AND (?2 OR is_draft = 0)",
    )
    .bind(repo_id)
    .bind(include_drafts)
    .fetch_one(pool)
    .await?)
}

pub async fn latest_release(pool: &SqlitePool, repo_id: i64) -> Result<Option<Release>> {
    Ok(sqlx::query_as::<_, Release>(&format!(
        "SELECT {RELEASE_COLUMNS} FROM releases r WHERE r.repo_id = ?1 AND r.is_latest = 1 AND r.is_draft = 0"
    ))
    .bind(repo_id)
    .fetch_optional(pool)
    .await?)
}

pub async fn list_release_assets(pool: &SqlitePool, release_id: i64) -> Result<Vec<ReleaseAsset>> {
    Ok(sqlx::query_as::<_, ReleaseAsset>(&format!(
        "SELECT {ASSET_COLUMNS} FROM release_assets WHERE release_id = ?1 ORDER BY name"
    ))
    .bind(release_id)
    .fetch_all(pool)
    .await?)
}

pub async fn get_release_asset_by_name(
    pool: &SqlitePool,
    release_id: i64,
    name: &str,
) -> Result<Option<ReleaseAsset>> {
    Ok(sqlx::query_as(&format!(
        "SELECT {ASSET_COLUMNS} FROM release_assets WHERE release_id=?1 AND name=?2"
    ))
    .bind(release_id)
    .bind(name)
    .fetch_optional(pool)
    .await?)
}

pub async fn list_releases_assets(
    pool: &SqlitePool,
    release_ids: &[i64],
) -> Result<Vec<ReleaseAsset>> {
    if release_ids.is_empty() {
        return Ok(Vec::new());
    }
    Ok(sqlx::query_as::<_, ReleaseAsset>(&format!(
        "SELECT {ASSET_COLUMNS} FROM release_assets WHERE release_id IN (SELECT value FROM json_each(?1)) ORDER BY release_id, name"
    )).bind(serde_json::to_string(release_ids)?).fetch_all(pool).await?)
}

pub async fn count_asset_download(pool: &SqlitePool, asset_id: i64) -> Result<()> {
    sqlx::query("UPDATE release_assets SET download_count = download_count + 1 WHERE id = ?1")
        .bind(asset_id)
        .execute(pool)
        .await?;
    Ok(())
}
