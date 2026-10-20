//! Uploaded file records. A file is pending until its bytes are confirmed,
//! then active; pending uploads expire.
use crate::backend::db::files::{
    AdmissionError, DeleteFileOutcome, DeleteUnreferencedOutcome, ReserveUpload,
    IN_FLIGHT_PUT_GRACE_SECONDS, PUT_LIFETIME_SECONDS,
};
use sqlx::{Sqlite, SqlitePool, Transaction};
use std::net::IpAddr;
use uuid::Uuid;

use super::models::{File, FileReasonLimit};

/// Checks the declaration and the per-reason quotas, then records a pending
/// file and charges the quota, in one transaction.
pub async fn reserve_upload(
    pool: &SqlitePool,
    req: ReserveUpload<'_>,
) -> Result<File, AdmissionError> {
    if !req.valid() {
        return Err(AdmissionError::Invalid);
    }
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let user: Option<i64> = sqlx::query_scalar(
        "SELECT id FROM users WHERE id = ?1 AND NOT is_banned AND NOT must_change_password",
    )
    .bind(req.user_id)
    .fetch_optional(&mut *tx)
    .await?;
    if user.is_none() {
        return Err(AdmissionError::Invalid);
    }
    let limits =
        sqlx::query_as::<_, FileReasonLimit>("SELECT * FROM file_reason_limits WHERE reason = ?1")
            .bind(req.reason.as_str())
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(AdmissionError::PolicyUnavailable)?;
    if !req.allowed_by(&limits) {
        return Err(AdmissionError::Invalid);
    }
    let allowed: bool = sqlx::query_scalar(
        r#"SELECT COUNT(*) FILTER (WHERE uploaded_at > strftime('%Y-%m-%dT%H:%M:%f000Z','now','-1 hour')) < ?3
           AND COALESCE(SUM(file_size) FILTER (WHERE uploaded_at > strftime('%Y-%m-%dT%H:%M:%f000Z','now','-1 hour')), 0) <= ?5 - ?4
           AND COUNT(*) < ?6
           AND COALESCE(SUM(file_size), 0) <= ?7 - ?4
        FROM upload_rate_limit
        WHERE user_id = ?1 AND reason = ?2 AND uploaded_at > strftime('%Y-%m-%dT%H:%M:%f000Z','now','-24 hours')"#,
    )
    .bind(req.user_id)
    .bind(req.reason.as_str())
    .bind(i64::from(limits.max_uploads_per_hour))
    .bind(req.file_size)
    .bind(limits.max_bytes_per_hour)
    .bind(i64::from(limits.max_uploads_per_day))
    .bind(limits.max_bytes_per_day)
    .fetch_one(&mut *tx)
    .await?;
    if !allowed {
        return Err(AdmissionError::Quota);
    }
    let file_id = Uuid::new_v4();
    let file = sqlx::query_as::<_, File>(
        "INSERT INTO files (id, user_id, filename, original_name, content_type, file_size, sha256_hex, object_key, reason, visibility, upload_expires_at, put_expires_at)
         VALUES (?1,?2,?3,?3,?4,?5,?6,?7,?8,?9,strftime('%Y-%m-%dT%H:%M:%f000Z','now',printf('+%d seconds',?10)),strftime('%Y-%m-%dT%H:%M:%f000Z','now',printf('+%d seconds',?10)))
         RETURNING *",
    )
    .bind(file_id)
    .bind(req.user_id)
    .bind(req.filename)
    .bind(req.content_type)
    .bind(req.file_size)
    .bind(req.sha256_hex)
    .bind(format!("uploads/{file_id}"))
    .bind(req.reason.as_str())
    .bind(req.visibility.as_str())
    .bind(PUT_LIFETIME_SECONDS)
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO upload_rate_limit (user_id, file_id, file_size, reason) VALUES (?1,?2,?3,?4)",
    )
    .bind(req.user_id)
    .bind(file_id)
    .bind(req.file_size)
    .bind(req.reason.as_str())
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(file)
}

/// Files anyone may download: the owner's own, public ones, and avatars.
pub async fn get_downloadable_file(
    pool: &SqlitePool,
    file_id: Uuid,
    actor: Option<i64>,
) -> Result<Option<File>, sqlx::Error> {
    sqlx::query_as(
        r#"SELECT f.* FROM files f WHERE f.id = ?1 AND f.is_active AND (
            f.user_id = ?2 OR f.visibility = 'public'
            OR (f.reason = 'avatar' AND EXISTS (SELECT 1 FROM users u WHERE u.avatar_file_id = f.id))
            OR (f.reason = 'org_avatar' AND EXISTS (SELECT 1 FROM organizations o WHERE o.avatar_file_id = f.id)))"#,
    )
    .bind(file_id)
    .bind(actor)
    .fetch_optional(pool)
    .await
}

/// Records verification without publishing a form upload before picture assignment.
pub async fn verify_pending_upload(
    pool: &SqlitePool,
    file_id: Uuid,
    user_id: i64,
) -> Result<bool, sqlx::Error> {
    let changed = sqlx::query(
        "UPDATE files SET confirmed_at = strftime('%Y-%m-%dT%H:%M:%f000Z','now'), updated_at = strftime('%Y-%m-%dT%H:%M:%f000Z','now')
         WHERE id = ?1 AND user_id = ?2 AND NOT is_active AND upload_expires_at > strftime('%Y-%m-%dT%H:%M:%f000Z','now')",
    )
    .bind(file_id)
    .bind(user_id)
    .execute(pool)
    .await?;
    Ok(changed.rows_affected() == 1)
}

/// Marks the owner's pending, unexpired upload active.
pub async fn activate_upload(
    pool: &SqlitePool,
    file_id: Uuid,
    user_id: i64,
) -> Result<bool, sqlx::Error> {
    let changed = sqlx::query(
        "UPDATE files SET is_active = 1, confirmed_at = strftime('%Y-%m-%dT%H:%M:%f000Z','now'), upload_expires_at = NULL, updated_at = strftime('%Y-%m-%dT%H:%M:%f000Z','now')
         WHERE id = ?1 AND user_id = ?2 AND NOT is_active AND upload_expires_at > strftime('%Y-%m-%dT%H:%M:%f000Z','now')",
    )
    .bind(file_id)
    .bind(user_id)
    .execute(pool)
    .await?;
    Ok(changed.rows_affected() == 1)
}

/// Queues the file's bytes for deletion in the caller's transaction. The bytes wait
/// out the grace period after the PUT deadline: a PUT accepted before the deadline
/// can still be transferring, and a replayed one can recreate bytes already removed.
pub(crate) async fn queue_deletion(
    tx: &mut Transaction<'_, Sqlite>,
    file: &File,
) -> Result<(), sqlx::Error> {
    let delete_after = file
        .put_expires_at
        .unwrap_or(file.created_at + chrono::Duration::seconds(PUT_LIFETIME_SECONDS))
        + chrono::Duration::seconds(IN_FLIGHT_PUT_GRACE_SECONDS);
    sqlx::query(
        "INSERT INTO pending_object_deletions(object_key,delete_after) VALUES(?1,?2)
         ON CONFLICT(object_key) DO UPDATE SET delete_after=MAX(pending_object_deletions.delete_after,excluded.delete_after)",
    )
    .bind(&file.object_key)
    .bind(delete_after)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Removes the row and queues its bytes unless a user or organization selects it.
/// The caller's write transaction is serialized by `BEGIN IMMEDIATE`.
async fn delete_if_unselected(
    tx: &mut Transaction<'_, Sqlite>,
    file: &File,
) -> Result<bool, sqlx::Error> {
    let selected: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM users WHERE avatar_file_id=?1 UNION ALL SELECT 1 FROM organizations WHERE avatar_file_id=?1 UNION ALL SELECT 1 FROM release_assets WHERE file_id=?1)",
    )
    .bind(file.id)
    .fetch_one(&mut **tx)
    .await?;
    if selected {
        return Ok(false);
    }
    queue_deletion(tx, file).await?;
    sqlx::query("DELETE FROM files WHERE id=?1")
        .bind(file.id)
        .execute(&mut **tx)
        .await?;
    Ok(true)
}

/// Removes an upload inside its former holder's transaction, unless another holder still
/// references it.
pub(crate) async fn delete_unreferenced_file_in_transaction(
    tx: &mut Transaction<'_, Sqlite>,
    file_id: Uuid,
) -> Result<(), sqlx::Error> {
    if let Some(file) = sqlx::query_as::<_, File>("SELECT * FROM files WHERE id=?1")
        .bind(file_id)
        .fetch_optional(&mut **tx)
        .await?
    {
        delete_if_unselected(tx, &file).await?;
    }
    Ok(())
}

/// Removes a former release upload after its links were deleted, in the same transaction.
pub(crate) async fn delete_unlinked_release_file(
    tx: &mut Transaction<'_, Sqlite>,
    file_id: Uuid,
) -> Result<(), sqlx::Error> {
    let file = sqlx::query_as::<_, File>("SELECT * FROM files WHERE id=?1")
        .bind(file_id)
        .fetch_optional(&mut **tx)
        .await?;
    if let Some(file) = file {
        let linked: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM release_assets WHERE file_id=?1)")
                .bind(file_id)
                .fetch_one(&mut **tx)
                .await?;
        if !linked {
            delete_if_unselected(tx, &file).await?;
        }
    }
    Ok(())
}

/// Starts the pending window at the moment a direct PUT URL is issued, and
/// retains that capability deadline after confirmation.
pub async fn extend_direct_upload(
    pool: &SqlitePool,
    file_id: Uuid,
    user_id: i64,
    seconds: i64,
) -> Result<bool, sqlx::Error> {
    let changed = sqlx::query(
        "UPDATE files SET upload_expires_at=strftime('%Y-%m-%dT%H:%M:%f000Z','now',printf('+%d seconds',?3)),put_expires_at=strftime('%Y-%m-%dT%H:%M:%f000Z','now',printf('+%d seconds',?3)),updated_at=strftime('%Y-%m-%dT%H:%M:%f000Z','now')
         WHERE id=?1 AND user_id=?2 AND NOT is_active",
    )
    .bind(file_id)
    .bind(user_id)
    .bind(seconds)
    .execute(pool)
    .await?;
    Ok(changed.rows_affected() == 1)
}

/// Removes owned metadata and records durable object cleanup in one transaction.
/// A selected organization picture is controlled by the organization endpoint.
pub async fn delete_file(
    pool: &SqlitePool,
    file_id: Uuid,
    user_id: i64,
) -> Result<DeleteFileOutcome, sqlx::Error> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    if sqlx::query_scalar::<_, i64>(
        "SELECT id FROM organizations WHERE avatar_file_id=?1 ORDER BY id LIMIT 1",
    )
    .bind(file_id)
    .fetch_optional(&mut *tx)
    .await?
    .is_some()
    {
        return Ok(DeleteFileOutcome::ReferencedOrganizationAvatar);
    }
    let allowed = sqlx::query_scalar::<_, bool>(
        "SELECT NOT is_banned AND NOT must_change_password FROM users WHERE id=?1",
    )
    .bind(user_id)
    .fetch_optional(&mut *tx)
    .await?
    .unwrap_or(false);
    let Some(file) = sqlx::query_as::<_, File>("SELECT * FROM files WHERE id=?1 AND user_id=?2")
        .bind(file_id)
        .bind(user_id)
        .fetch_optional(&mut *tx)
        .await?
    else {
        return Ok(DeleteFileOutcome::NotFound);
    };
    if !allowed {
        return Ok(DeleteFileOutcome::Denied);
    }
    // A release asset's file is removed with the asset, through its release.
    if sqlx::query_scalar::<_, bool>("SELECT EXISTS(SELECT 1 FROM release_assets WHERE file_id=?1)")
        .bind(file_id)
        .fetch_one(&mut *tx)
        .await?
    {
        return Ok(DeleteFileOutcome::ReferencedReleaseAsset);
    }
    queue_deletion(&mut tx, &file).await?;
    sqlx::query("DELETE FROM files WHERE id=?1")
        .bind(file_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(DeleteFileOutcome::Deleted(Box::new(file)))
}

/// Deletes an unused picture of `user_id` only when no user or organization still
/// selects it.
pub async fn delete_unreferenced_file(
    pool: &SqlitePool,
    file_id: Uuid,
    user_id: i64,
) -> Result<DeleteUnreferencedOutcome, sqlx::Error> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let Some(file) = sqlx::query_as::<_, File>("SELECT * FROM files WHERE id=?1 AND user_id=?2")
        .bind(file_id)
        .bind(user_id)
        .fetch_optional(&mut *tx)
        .await?
    else {
        return Ok(DeleteUnreferencedOutcome::NotFound);
    };
    if !delete_if_unselected(&mut tx, &file).await? {
        return Ok(DeleteUnreferencedOutcome::Referenced);
    }
    tx.commit().await?;
    Ok(DeleteUnreferencedOutcome::Deleted(Box::new(file)))
}

/// Pending uploads whose upload window has passed.
pub async fn expired_uploads(pool: &SqlitePool, limit: i64) -> Result<Vec<File>, sqlx::Error> {
    sqlx::query_as("SELECT * FROM files WHERE NOT is_active AND upload_expires_at <= strftime('%Y-%m-%dT%H:%M:%f000Z','now') ORDER BY upload_expires_at LIMIT ?1")
        .bind(limit)
        .fetch_all(pool)
        .await
}

pub async fn delete_expired_upload(
    pool: &SqlitePool,
    file_id: Uuid,
) -> Result<Option<File>, sqlx::Error> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let file = sqlx::query_as::<_, File>("SELECT * FROM files WHERE id=?1 AND NOT is_active AND upload_expires_at <= strftime('%Y-%m-%dT%H:%M:%f000Z','now')")
        .bind(file_id)
        .fetch_optional(&mut *tx)
        .await?;
    let Some(file) = file else {
        return Ok(None);
    };
    queue_deletion(&mut tx, &file).await?;
    sqlx::query("DELETE FROM files WHERE id=?1")
        .bind(file_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Some(file))
}

pub async fn due_object_deletions(
    pool: &SqlitePool,
    limit: i64,
) -> Result<Vec<String>, sqlx::Error> {
    sqlx::query_scalar("SELECT object_key FROM pending_object_deletions WHERE delete_after <= strftime('%Y-%m-%dT%H:%M:%f000Z','now') ORDER BY delete_after,object_key LIMIT ?1")
        .bind(limit)
        .fetch_all(pool)
        .await
}

pub async fn finish_object_deletion(
    pool: &SqlitePool,
    object_key: &str,
) -> Result<bool, sqlx::Error> {
    Ok(sqlx::query("DELETE FROM pending_object_deletions WHERE object_key=?1 AND delete_after <= strftime('%Y-%m-%dT%H:%M:%f000Z','now')")
        .bind(object_key)
        .execute(pool)
        .await?
        .rows_affected()
        == 1)
}

/// Moves a key whose bytes could not be removed `seconds` into the future, so a key
/// that keeps failing does not take a batch slot on every sweep.
pub async fn postpone_object_deletion(
    pool: &SqlitePool,
    object_key: &str,
    seconds: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE pending_object_deletions SET delete_after=strftime('%Y-%m-%dT%H:%M:%f000Z','now',printf('+%d seconds',?2)) WHERE object_key=?1")
        .bind(object_key)
        .bind(seconds)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn get_file_by_id(pool: &SqlitePool, file_id: Uuid) -> Result<Option<File>, sqlx::Error> {
    sqlx::query_as("SELECT * FROM files WHERE id = ?1")
        .bind(file_id)
        .fetch_optional(pool)
        .await
}

pub async fn get_user_file(
    pool: &SqlitePool,
    file_id: Uuid,
    user_id: i64,
) -> Result<Option<File>, sqlx::Error> {
    sqlx::query_as("SELECT * FROM files WHERE id = ?1 AND user_id = ?2 AND is_active")
        .bind(file_id)
        .bind(user_id)
        .fetch_optional(pool)
        .await
}

pub async fn remove_expired_transfer_rows(pool: &SqlitePool) -> Result<u64, sqlx::Error> {
    let mut removed = 0;
    for sql in [
        "DELETE FROM upload_rate_limit WHERE id IN (SELECT id FROM upload_rate_limit WHERE uploaded_at < strftime('%Y-%m-%dT%H:%M:%f000Z','now','-730 days') ORDER BY uploaded_at LIMIT 500)",
        "DELETE FROM download_rate_limit WHERE id IN (SELECT id FROM download_rate_limit WHERE downloaded_at < strftime('%Y-%m-%dT%H:%M:%f000Z','now','-30 days') ORDER BY downloaded_at LIMIT 500)",
    ] {
        // Each statement deletes at most 500 rows, so a short batch means none are left.
        for _ in 0..20 {
            let batch = sqlx::query(sql).execute(pool).await?.rows_affected();
            removed += batch;
            if batch < 500 {
                break;
            }
        }
    }
    Ok(removed)
}

pub async fn record_download(
    pool: &SqlitePool,
    user_id: i64,
    file_id: Uuid,
    reason: Option<&str>,
    ip: Option<IpAddr>,
) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO download_rate_limit (user_id, file_id, reason, ip_address) VALUES (?1,?2,?3,?4)")
        .bind(user_id).bind(file_id).bind(reason).bind(ip.map(super::inet::address)).execute(pool).await?;
    Ok(())
}

/// Overflow is an error, not truncated/saturated successful accounting.
pub async fn get_user_total_storage(pool: &SqlitePool, user_id: i64) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT COALESCE(SUM(file_size),0) FROM files WHERE user_id = ?1 AND is_active",
    )
    .bind(user_id)
    .fetch_one(pool)
    .await
}

pub async fn get_user_file_stats(
    pool: &SqlitePool,
    user_id: i64,
) -> Result<Option<super::models::UserFileStats>, sqlx::Error> {
    // Aggregate each relation separately. Joining the relations first would multiply
    // the storage and upload sums by the join counts.
    sqlx::query_as(r#"WITH active_files AS (
        SELECT COUNT(*) AS total_files,COALESCE(SUM(file_size),0) AS total_bytes,
            COUNT(*) FILTER(WHERE reason='avatar') AS avatar_count,
            COUNT(*) FILTER(WHERE reason='release_asset') AS release_asset_count,
            COUNT(*) FILTER(WHERE reason='issue_attachment') AS issue_attachment_count,
            COUNT(*) FILTER(WHERE reason='pr_attachment') AS pr_attachment_count,
            COUNT(*) FILTER(WHERE reason='readme_image') AS readme_image_count,
            COUNT(*) FILTER(WHERE reason='repo_asset') AS repo_asset_count,
            COUNT(*) FILTER(WHERE reason='org_avatar') AS org_avatar_count,
            COUNT(*) FILTER(WHERE reason='other') AS other_count
        FROM files WHERE user_id=?1 AND is_active
    ), charged(bytes) AS (
        SELECT file_size FROM upload_rate_limit WHERE user_id=?1 AND uploaded_at>strftime('%Y-%m-%dT%H:%M:%f000Z','now','-24 hours')
    ), uploads AS (SELECT COUNT(*) AS uploads_24h,COALESCE(SUM(bytes),0) AS bytes_24h FROM charged)
    SELECT u.id AS user_id,u.username,f.*,uploads.*,
        (SELECT COUNT(*) FROM download_rate_limit d WHERE d.user_id=?1 AND d.downloaded_at>strftime('%Y-%m-%dT%H:%M:%f000Z','now','-24 hours')) AS downloads_24h
    FROM users u CROSS JOIN active_files f CROSS JOIN uploads WHERE u.id=?1"#)
        .bind(user_id).fetch_optional(pool).await
}
