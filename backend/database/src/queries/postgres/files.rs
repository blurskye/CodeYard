use crate::models::File;
use crate::queries::files::IN_FLIGHT_PUT_GRACE_SECONDS;
use crate::queries::files::PUT_LIFETIME_SECONDS;
use sqlx::Postgres;
use sqlx::Transaction;
use uuid::Uuid;
/// Queues the file's bytes for deletion in the caller's transaction. The bytes wait
/// out the grace period after the PUT deadline: a PUT accepted before the deadline
/// can still be transferring, and a replayed one can recreate bytes already removed.
/// They also stay for the restore window, so a restored database still finds them.
pub async fn queue_deletion(
    tx: &mut Transaction<'_, Postgres>,
    file: &File,
) -> Result<(), sqlx::Error> {
    let delete_after = file
        .put_expires_at
        .unwrap_or(file.created_at + chrono::Duration::seconds(PUT_LIFETIME_SECONDS))
        + chrono::Duration::seconds(IN_FLIGHT_PUT_GRACE_SECONDS);
    let delete_after = delete_after.max(
        chrono::Utc::now()
            + chrono::Duration::seconds(crate::queries::files::RESTORE_WINDOW_SECONDS),
    );
    sqlx::query(
        "INSERT INTO pending_object_deletions(object_key,delete_after) VALUES($1,$2)
         ON CONFLICT(object_key) DO UPDATE SET delete_after=GREATEST(pending_object_deletions.delete_after,EXCLUDED.delete_after)",
    )
    .bind(&file.object_key)
    .bind(delete_after)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Removes the row and queues its bytes unless a user or organization selects it.
/// The caller holds the row lock: a selection either committed before this check,
/// or waits for the lock and then finds the row gone.
pub async fn delete_if_unselected(
    tx: &mut Transaction<'_, Postgres>,
    file: &File,
) -> Result<bool, sqlx::Error> {
    let selected: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM users WHERE avatar_file_id=$1 UNION ALL SELECT 1 FROM organizations WHERE avatar_file_id=$1 UNION ALL SELECT 1 FROM release_assets WHERE file_id=$1)",
    )
    .bind(file.id)
    .fetch_one(&mut **tx)
    .await?;
    if selected {
        return Ok(false);
    }
    queue_deletion(tx, file).await?;
    sqlx::query("DELETE FROM files WHERE id=$1")
        .bind(file.id)
        .execute(&mut **tx)
        .await?;
    Ok(true)
}

/// Removes an upload inside its former holder's transaction, unless another holder still
/// references it. The file lock also excludes new asset links while it is being removed.
pub async fn delete_unreferenced_file_in_transaction(
    tx: &mut Transaction<'_, Postgres>,
    file_id: Uuid,
) -> Result<(), sqlx::Error> {
    if let Some(file) = sqlx::query_as::<_, File>("SELECT * FROM files WHERE id=$1 FOR UPDATE")
        .bind(file_id)
        .fetch_optional(&mut **tx)
        .await?
    {
        delete_if_unselected(tx, &file).await?;
    }
    Ok(())
}
