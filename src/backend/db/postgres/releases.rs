mod notes;
mod read;
use crate::backend::db::models::{File, Release, ReleaseAsset, ReleaseRow};
use crate::backend::db::releases::{
    NewReleaseAsset, ReleaseChanges, ReleaseFields, ReleaseRefusal,
};
use anyhow::Result;
pub use notes::release_note_rows;
pub use read::read_release_snapshot;
use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

const RELEASE_COLUMNS: &str = "r.id, r.repo_id, r.tag_name, r.target_commitish, r.name, r.body, r.is_draft, r.is_immutable, r.is_prerelease, r.is_latest, r.author_id, r.created_at, r.published_at";

const ROW_SELECT: &str = "SELECT r.id, r.repo_id, r.tag_name, r.target_commitish, r.name, r.body, r.is_draft, r.is_immutable, r.is_prerelease, r.is_latest, r.author_id, r.created_at, r.published_at, u.username AS author FROM releases r JOIN users u ON u.id = r.author_id";

const ASSET_COLUMNS: &str =
    "id, release_id, file_id, name, label, content_type, size, download_count, created_at, updated_at";

/// Writes lock the repository row, so the latest flag and the tag check see one writer at a time.
async fn begin_write(
    pool: &PgPool,
    actor_id: i64,
    repo_id: i64,
) -> Result<Transaction<'_, Postgres>> {
    let mut tx = pool.begin().await?;
    anyhow::ensure!(
        super::discussions::lock_exclusive_write_authority(&mut tx, actor_id, repo_id).await?,
        ReleaseRefusal::Denied
    );
    Ok(tx)
}

async fn release_in_repo(
    tx: &mut Transaction<'_, Postgres>,
    repo_id: i64,
    release_id: i64,
) -> Result<Release> {
    sqlx::query_as::<_, Release>(&format!(
        "SELECT {RELEASE_COLUMNS} FROM releases r WHERE r.id = $1 AND r.repo_id = $2"
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
    tx: &mut Transaction<'_, Postgres>,
    repo_id: i64,
    release_id: i64,
    fields: &ReleaseFields,
) -> Result<()> {
    if matches!(
        fields.make_latest,
        crate::backend::db::releases::LatestSelection::Legacy
    ) {
        let rows = sqlx::query_as("SELECT id,tag_name,created_at FROM releases WHERE repo_id=$1 AND NOT is_draft AND NOT is_prerelease")
            .bind(repo_id).fetch_all(&mut **tx).await?;
        let candidate = crate::backend::db::releases::legacy_latest(&rows);
        sqlx::query("UPDATE releases SET is_latest=FALSE WHERE repo_id=$1 AND is_latest")
            .bind(repo_id)
            .execute(&mut **tx)
            .await?;
        sqlx::query("UPDATE releases SET is_latest=TRUE WHERE id=$1")
            .bind(candidate)
            .execute(&mut **tx)
            .await?;
        return Ok(());
    }
    let wants = matches!(
        fields.make_latest,
        crate::backend::db::releases::LatestSelection::Promote
    ) && !fields.is_draft
        && !fields.is_prerelease;
    if wants {
        sqlx::query(
            "UPDATE releases SET is_latest = FALSE WHERE repo_id = $1 AND is_latest AND id <> $2",
        )
        .bind(repo_id)
        .bind(release_id)
        .execute(&mut **tx)
        .await?;
    }
    sqlx::query("UPDATE releases SET is_latest = $1 WHERE id = $2")
        .bind(wants)
        .bind(release_id)
        .execute(&mut **tx)
        .await?;
    if !wants {
        elect_latest(tx, repo_id, release_id).await?;
    }
    Ok(())
}

/// Marks the newest published stable release other than `except` as latest when no release
/// holds the flag, so deleting or demoting the latest release hands the flag on.
async fn elect_latest(tx: &mut Transaction<'_, Postgres>, repo_id: i64, except: i64) -> Result<()> {
    sqlx::query(
        "UPDATE releases SET is_latest = TRUE WHERE id = (
           SELECT id FROM releases
           WHERE repo_id = $1 AND id <> $2 AND NOT is_draft AND NOT is_prerelease
             AND NOT EXISTS (SELECT 1 FROM releases WHERE repo_id = $1 AND is_latest)
           ORDER BY created_at DESC, id DESC LIMIT 1)",
    )
    .bind(repo_id)
    .bind(except)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

pub async fn create_release(
    pool: &PgPool,
    actor_id: i64,
    repo_id: i64,
    tag: &str,
    target: &str,
    fields: &ReleaseFields,
) -> Result<Release> {
    create_release_with_publication(pool, actor_id, repo_id, tag, target, fields, None).await
}

pub async fn create_release_with_publication(
    pool: &PgPool,
    actor_id: i64,
    repo_id: i64,
    tag: &str,
    target: &str,
    fields: &ReleaseFields,
    publication: Option<&crate::backend::db::repositories::immutable_releases::Publication>,
) -> Result<Release> {
    let mut tx = begin_write(pool, actor_id, repo_id).await?;
    let taken: Option<i64> =
        sqlx::query_scalar("SELECT id FROM releases WHERE repo_id = $1 AND tag_name = $2")
            .bind(repo_id)
            .bind(tag)
            .fetch_optional(&mut *tx)
            .await?;
    anyhow::ensure!(taken.is_none(), ReleaseRefusal::TagTaken);
    super::repositories::immutable_releases::assert_available(&mut tx, repo_id, tag).await?;
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO releases (repo_id, tag_name, target_commitish, name, body, is_draft, is_prerelease, author_id, published_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8,
                 CASE WHEN $6 THEN NULL ELSE NOW() END)
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
    super::repositories::immutable_releases::freeze(&mut tx, &release, true, publication).await?;
    let release = release_in_repo(&mut tx, repo_id, id).await?;
    tx.commit().await?;
    Ok(release)
}

pub async fn update_release(
    pool: &PgPool,
    actor_id: i64,
    repo_id: i64,
    release_id: i64,
    fields: &ReleaseChanges,
) -> Result<(Release, bool)> {
    update_release_with_publication(pool, actor_id, repo_id, release_id, fields, None).await
}

pub async fn update_release_with_publication(
    pool: &PgPool,
    actor_id: i64,
    repo_id: i64,
    release_id: i64,
    fields: &ReleaseChanges,
    publication: Option<&crate::backend::db::repositories::immutable_releases::Publication>,
) -> Result<(Release, bool)> {
    let mut tx = begin_write(pool, actor_id, repo_id).await?;
    let current = release_in_repo(&mut tx, repo_id, release_id).await?;
    if let Some(expected) = &fields.expected_revision {
        anyhow::ensure!(
            *expected == crate::backend::db::releases::release_revision(&current),
            ReleaseRefusal::Conflict
        );
    }
    anyhow::ensure!(
        fields
            .expected_draft
            .is_none_or(|draft| draft == current.is_draft),
        ReleaseRefusal::Conflict
    );
    anyhow::ensure!(
        !current.is_immutable || fields.is_draft != Some(true),
        ReleaseRefusal::Immutable
    );
    if let Some(tag) = &fields.tag {
        anyhow::ensure!(
            !current.is_immutable || tag == &current.tag_name,
            ReleaseRefusal::Immutable
        );
        let taken: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM releases WHERE repo_id=$1 AND tag_name=$2 AND id<>$3)",
        )
        .bind(repo_id)
        .bind(tag)
        .bind(release_id)
        .fetch_one(&mut *tx)
        .await?;
        anyhow::ensure!(!taken, ReleaseRefusal::TagTaken);
        if tag != &current.tag_name {
            super::repositories::immutable_releases::assert_available(&mut tx, repo_id, tag)
                .await?;
        }
    }
    let tag = fields
        .tag
        .clone()
        .unwrap_or_else(|| current.tag_name.clone());
    let target = fields
        .target
        .clone()
        .unwrap_or_else(|| current.target_commitish.clone());
    let fields = fields.apply_to(&current);
    // A release first published keeps its date. A draft gets one when it is published.
    sqlx::query(
        "UPDATE releases SET name = $1, body = $2, is_draft = $3, is_prerelease = $4,
             published_at = CASE WHEN $3 THEN NULL
                                 WHEN published_at IS NULL THEN NOW()
                                 ELSE published_at END
         , tag_name = $6, target_commitish = $7 WHERE id = $5",
    )
    .bind(&fields.name)
    .bind(&fields.body)
    .bind(fields.is_draft)
    .bind(fields.is_prerelease)
    .bind(release_id)
    .bind(tag)
    .bind(target)
    .execute(&mut *tx)
    .await?;
    set_latest_flag(&mut tx, repo_id, release_id, &fields).await?;
    let release = release_in_repo(&mut tx, repo_id, release_id).await?;
    let published = current.is_draft && !release.is_draft;
    super::repositories::immutable_releases::freeze(
        &mut tx,
        &release,
        current.is_draft,
        publication,
    )
    .await?;
    let release = release_in_repo(&mut tx, repo_id, release_id).await?;
    tx.commit().await?;
    Ok((release, published))
}

pub async fn delete_release(
    pool: &PgPool,
    actor_id: i64,
    repo_id: i64,
    release_id: i64,
) -> Result<Vec<File>> {
    let mut tx = begin_write(pool, actor_id, repo_id).await?;
    release_in_repo(&mut tx, repo_id, release_id).await?;
    let files: Vec<Uuid> = sqlx::query_scalar(
        "SELECT DISTINCT file_id FROM release_assets WHERE release_id = $1 ORDER BY file_id",
    )
    .bind(release_id)
    .fetch_all(&mut *tx)
    .await?;
    sqlx::query("DELETE FROM reactions WHERE subject_kind='release' AND subject_id=$1")
        .bind(release_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM releases WHERE id = $1")
        .bind(release_id)
        .execute(&mut *tx)
        .await?;
    elect_latest(&mut tx, repo_id, release_id).await?;
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
    pool: &PgPool,
    actor_id: i64,
    credential: &crate::backend::db::users::accounts::AccountCredential<'_>,
    repo_id: i64,
    release_id: i64,
    asset: &NewReleaseAsset<'_>,
    token: Option<i64>,
) -> Result<ReleaseAsset> {
    let mut tx = begin_write(pool, actor_id, repo_id).await?;
    anyhow::ensure!(
        super::users::transitions::credential_live(&mut tx, actor_id, credential, true).await?,
        ReleaseRefusal::Denied
    );
    if let Some(token) = token {
        anyhow::ensure!(
            super::users::tokens::repository_write_on(
                &mut tx, token, actor_id, repo_id, "contents"
            )
            .await?,
            ReleaseRefusal::Denied
        );
    }
    let release = release_in_repo(&mut tx, repo_id, release_id).await?;
    anyhow::ensure!(!release.is_immutable, ReleaseRefusal::Immutable);
    let taken: Option<i64> =
        sqlx::query_scalar("SELECT id FROM release_assets WHERE release_id = $1 AND name = $2")
            .bind(release_id)
            .bind(asset.name)
            .fetch_optional(&mut *tx)
            .await?;
    anyhow::ensure!(taken.is_none(), ReleaseRefusal::AssetNameTaken);
    let held: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM release_assets WHERE release_id = $1")
        .bind(release_id)
        .fetch_one(&mut *tx)
        .await?;
    anyhow::ensure!(
        held < crate::backend::db::releases::MAX_RELEASE_ASSETS,
        ReleaseRefusal::TooManyAssets
    );
    let activated = sqlx::query(
        "UPDATE files SET is_active=TRUE, upload_expires_at=NULL
         WHERE id=$1 AND user_id=$2 AND reason='release_asset' AND NOT is_active
           AND confirmed_at IS NOT NULL AND upload_expires_at > NOW()",
    )
    .bind(asset.file_id)
    .bind(actor_id)
    .execute(&mut *tx)
    .await?;
    anyhow::ensure!(activated.rows_affected() == 1, ReleaseRefusal::NotFound);
    let asset = sqlx::query_as::<_, ReleaseAsset>(&format!(
        "INSERT INTO release_assets (release_id, file_id, name, label, content_type, size)
         VALUES ($1, $2, $3, $4, $5, $6) RETURNING {ASSET_COLUMNS}"
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

pub async fn update_release_asset(
    pool: &PgPool,
    actor: i64,
    repo_id: i64,
    asset_id: i64,
    changes: &crate::backend::db::releases::AssetChanges,
) -> Result<ReleaseAsset> {
    let mut tx = begin_write(pool, actor, repo_id).await?;
    let release_id: i64 = sqlx::query_scalar("SELECT a.release_id FROM release_assets a JOIN releases r ON r.id=a.release_id WHERE a.id=$1 AND r.repo_id=$2").bind(asset_id).bind(repo_id).fetch_optional(&mut *tx).await?.ok_or(ReleaseRefusal::NotFound)?;
    let release = release_in_repo(&mut tx, repo_id, release_id).await?;
    anyhow::ensure!(!release.is_immutable, ReleaseRefusal::Immutable);
    let taken: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM release_assets WHERE release_id=$1 AND name=$2 AND id<>$3)",
    )
    .bind(release_id)
    .bind(&changes.name)
    .bind(asset_id)
    .fetch_one(&mut *tx)
    .await?;
    anyhow::ensure!(!taken, ReleaseRefusal::AssetNameTaken);
    let asset = sqlx::query_as::<_, ReleaseAsset>(&format!("UPDATE release_assets SET name=COALESCE($2,name), label=CASE WHEN $3 THEN $4 ELSE label END, updated_at=NOW() WHERE id=$1 RETURNING {ASSET_COLUMNS}"))
        .bind(asset_id).bind(&changes.name).bind(changes.label.is_some()).bind(changes.label.as_ref().and_then(|label| label.as_deref())).fetch_one(&mut *tx).await?;
    tx.commit().await?;
    Ok(asset)
}

pub async fn delete_release_asset(
    pool: &PgPool,
    actor_id: i64,
    repo_id: i64,
    release_id: i64,
    asset_id: i64,
) -> Result<Option<File>> {
    let mut tx = begin_write(pool, actor_id, repo_id).await?;
    let release = release_in_repo(&mut tx, repo_id, release_id).await?;
    anyhow::ensure!(!release.is_immutable, ReleaseRefusal::Immutable);
    let file: Option<Uuid> = sqlx::query_scalar(
        "DELETE FROM release_assets WHERE id = $1 AND release_id = $2 RETURNING file_id",
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

async fn delete_asset_file(tx: &mut Transaction<'_, Postgres>, id: Uuid) -> Result<Option<File>> {
    let file = sqlx::query_as::<_, File>(
        "SELECT * FROM files WHERE id=$1 AND reason='release_asset' FOR UPDATE",
    )
    .bind(id)
    .fetch_optional(&mut **tx)
    .await?;
    if let Some(file) = &file {
        let used: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM release_assets WHERE file_id=$1)")
                .bind(id)
                .fetch_one(&mut **tx)
                .await?;
        if used {
            return Ok(None);
        }
        super::files::queue_deletion(tx, file).await?;
        sqlx::query("DELETE FROM files WHERE id=$1")
            .bind(id)
            .execute(&mut **tx)
            .await?;
    }
    Ok(file)
}

pub async fn get_release_by_tag(
    pool: &PgPool,
    repo_id: i64,
    tag: &str,
) -> Result<Option<ReleaseRow>> {
    Ok(sqlx::query_as::<_, ReleaseRow>(&format!(
        "{ROW_SELECT} WHERE r.repo_id = $1 AND r.tag_name = $2"
    ))
    .bind(repo_id)
    .bind(tag)
    .fetch_optional(pool)
    .await?)
}

pub async fn get_release(
    pool: &PgPool,
    repo_id: i64,
    release_id: i64,
) -> Result<Option<ReleaseRow>> {
    Ok(
        sqlx::query_as::<_, ReleaseRow>(&format!(
            "{ROW_SELECT} WHERE r.repo_id = $1 AND r.id = $2"
        ))
        .bind(repo_id)
        .bind(release_id)
        .fetch_optional(pool)
        .await?,
    )
}

pub async fn list_releases(
    pool: &PgPool,
    repo_id: i64,
    include_drafts: bool,
    limit: i64,
    offset: i64,
) -> Result<Vec<ReleaseRow>> {
    list_releases_ordered(
        pool,
        repo_id,
        include_drafts,
        crate::backend::db::releases::ReleaseOrder::Published,
        limit,
        offset,
    )
    .await
}
pub async fn list_releases_ordered(
    pool: &PgPool,
    repo_id: i64,
    include_drafts: bool,
    order: crate::backend::db::releases::ReleaseOrder,
    limit: i64,
    offset: i64,
) -> Result<Vec<ReleaseRow>> {
    let order = order.sql();
    Ok(sqlx::query_as::<_, ReleaseRow>(&format!(
        "{ROW_SELECT} WHERE r.repo_id = $1 AND ($2 OR NOT r.is_draft)
         ORDER BY {order} LIMIT $3 OFFSET $4"
    ))
    .bind(repo_id)
    .bind(include_drafts)
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await?)
}

pub async fn list_releases_by_creation(
    pool: &PgPool,
    repo_id: i64,
    include_drafts: bool,
    ascending: bool,
    limit: i64,
    offset: i64,
) -> Result<Vec<ReleaseRow>> {
    let direction = if ascending { "ASC" } else { "DESC" };
    Ok(sqlx::query_as::<_, ReleaseRow>(&format!(
        "{ROW_SELECT} WHERE r.repo_id = $1 AND ($2 OR NOT r.is_draft)
         ORDER BY r.created_at {direction}, r.id {direction} LIMIT $3 OFFSET $4"
    ))
    .bind(repo_id)
    .bind(include_drafts)
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await?)
}

pub async fn count_releases(pool: &PgPool, repo_id: i64, include_drafts: bool) -> Result<i64> {
    Ok(sqlx::query_scalar(
        "SELECT COUNT(*) FROM releases WHERE repo_id = $1 AND ($2 OR NOT is_draft)",
    )
    .bind(repo_id)
    .bind(include_drafts)
    .fetch_one(pool)
    .await?)
}

pub async fn latest_release(pool: &PgPool, repo_id: i64) -> Result<Option<Release>> {
    Ok(sqlx::query_as::<_, Release>(&format!(
        "SELECT {RELEASE_COLUMNS} FROM releases r WHERE r.repo_id = $1 AND r.is_latest AND NOT r.is_draft"
    ))
    .bind(repo_id)
    .fetch_optional(pool)
    .await?)
}

pub async fn list_asset_files(
    pool: &PgPool,
    release_id: i64,
) -> Result<Vec<crate::backend::db::releases::ReleaseAssetFile>> {
    Ok(sqlx::query_as("SELECT a.*,f.sha256_hex,u.id AS uploader_id,u.username AS uploader_name,u.is_admin AS uploader_is_admin FROM release_assets a JOIN files f ON f.id=a.file_id LEFT JOIN users u ON u.id=f.user_id WHERE a.release_id=$1 ORDER BY a.name,a.id")
        .bind(release_id).fetch_all(pool).await?)
}

pub async fn list_release_assets(pool: &PgPool, release_id: i64) -> Result<Vec<ReleaseAsset>> {
    Ok(sqlx::query_as::<_, ReleaseAsset>(&format!(
        "SELECT {ASSET_COLUMNS} FROM release_assets WHERE release_id = $1 ORDER BY name"
    ))
    .bind(release_id)
    .fetch_all(pool)
    .await?)
}

pub async fn get_repository_release_asset(
    pool: &PgPool,
    repo_id: i64,
    asset_id: i64,
) -> Result<Option<ReleaseAsset>> {
    Ok(sqlx::query_as("SELECT a.* FROM release_assets a JOIN releases r ON r.id=a.release_id WHERE r.repo_id=$1 AND a.id=$2")
        .bind(repo_id).bind(asset_id).fetch_optional(pool).await?)
}

pub async fn download_asset(
    pool: &PgPool,
    viewer: Option<i64>,
    repo: i64,
    tag: Option<&str>,
    name: Option<&str>,
    asset: Option<i64>,
) -> Result<Option<ReleaseAsset>> {
    let readable = super::repositories::access::readable_condition();
    let access = super::repositories::access::USER_REPO_ACCESS_SQL;
    Ok(sqlx::query_as(&format!("SELECT a.* FROM release_assets a JOIN releases rel ON rel.id=a.release_id JOIN repositories r ON r.id=rel.repo_id LEFT JOIN users u ON u.id=$1 WHERE r.id=$2 AND {readable} AND (NOT rel.is_draft OR ({access})>=3 OR (r.owner_kind='user' AND u.is_admin)) AND ($3::text IS NULL OR rel.tag_name=$3) AND ($4::text IS NULL OR a.name=$4) AND ($5::bigint IS NULL OR a.id=$5)"))
        .bind(viewer).bind(repo).bind(tag).bind(name).bind(asset).fetch_optional(pool).await?)
}

pub async fn get_release_asset_by_name(
    pool: &PgPool,
    release_id: i64,
    name: &str,
) -> Result<Option<ReleaseAsset>> {
    Ok(sqlx::query_as(&format!(
        "SELECT {ASSET_COLUMNS} FROM release_assets WHERE release_id=$1 AND name=$2"
    ))
    .bind(release_id)
    .bind(name)
    .fetch_optional(pool)
    .await?)
}

pub async fn list_releases_assets(pool: &PgPool, release_ids: &[i64]) -> Result<Vec<ReleaseAsset>> {
    if release_ids.is_empty() {
        return Ok(Vec::new());
    }
    Ok(sqlx::query_as::<_, ReleaseAsset>(&format!(
        "SELECT {ASSET_COLUMNS} FROM release_assets WHERE release_id = ANY($1) ORDER BY release_id, name"
    )).bind(release_ids).fetch_all(pool).await?)
}

pub async fn count_asset_download(pool: &PgPool, asset_id: i64) -> Result<()> {
    sqlx::query("UPDATE release_assets SET download_count = download_count + 1 WHERE id = $1")
        .bind(asset_id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn release_discussion(
    pool: &PgPool,
    release_id: i64,
) -> Result<Option<crate::backend::db::releases::ReleaseDiscussion>> {
    Ok(sqlx::query_as("SELECT l.category_id, d.number FROM release_discussions l LEFT JOIN discussions d ON d.id = l.discussion_id WHERE l.release_id = $1")
        .bind(release_id)
        .fetch_optional(pool)
        .await?)
}

pub async fn set_release_discussion_category(
    pool: &PgPool,
    release_id: i64,
    category_id: i64,
) -> Result<()> {
    sqlx::query("INSERT INTO release_discussions (release_id, category_id) VALUES ($1, $2) ON CONFLICT (release_id) DO UPDATE SET category_id = excluded.category_id WHERE release_discussions.discussion_id IS NULL")
        .bind(release_id)
        .bind(category_id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn link_release_discussion(
    pool: &PgPool,
    release_id: i64,
    discussion_id: i64,
) -> Result<bool> {
    Ok(sqlx::query("UPDATE release_discussions SET discussion_id = $2 WHERE release_id = $1 AND discussion_id IS NULL")
        .bind(release_id)
        .bind(discussion_id)
        .execute(pool)
        .await?
        .rows_affected()
        == 1)
}
