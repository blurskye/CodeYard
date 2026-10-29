use crate::backend::db::{
    assignees::AssignedItem,
    pins::{scope, Pin, PinRefusal},
};
use anyhow::Result;
use sqlx::SqlitePool;
pub async fn get_pin(pool: &SqlitePool, item: AssignedItem) -> Result<Option<Pin>> {
    let s = scope(item);
    Ok(sqlx::query_as(&format!(
        "SELECT comment_id,pinned_by,pinned_at FROM {} WHERE {}=?1",
        s.pins, s.foreign
    ))
    .bind(s.id)
    .fetch_optional(pool)
    .await?)
}
pub async fn set_pin(
    pool: &SqlitePool,
    actor: i64,
    repo: i64,
    item: AssignedItem,
    comment: i64,
    pinned: bool,
) -> Result<()> {
    let s = scope(item);
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    anyhow::ensure!(
        super::discussions::lock_write_authority(&mut tx, actor, repo).await?,
        PinRefusal::Denied
    );
    let present: bool = sqlx::query_scalar(&format!(
        "SELECT EXISTS (SELECT 1 FROM {} WHERE id=?1 AND repo_id=?2)",
        s.parent
    ))
    .bind(s.id)
    .bind(repo)
    .fetch_one(&mut *tx)
    .await?;
    anyhow::ensure!(present, PinRefusal::Missing);
    let present: bool = sqlx::query_scalar(&format!(
        "SELECT EXISTS (SELECT 1 FROM {} WHERE id=?1 AND {}=?2 {})",
        s.comments,
        s.foreign,
        if s.pull {
            "AND path IS NULL AND review_id IS NULL"
        } else {
            ""
        }
    ))
    .bind(comment)
    .bind(s.id)
    .fetch_one(&mut *tx)
    .await?;
    anyhow::ensure!(present, PinRefusal::Missing);
    let current: Option<i64> = sqlx::query_scalar(&format!(
        "SELECT comment_id FROM {} WHERE {}=?1",
        s.pins, s.foreign
    ))
    .bind(s.id)
    .fetch_optional(&mut *tx)
    .await?;
    if pinned {
        anyhow::ensure!(current.is_none_or(|id| id == comment), PinRefusal::Occupied);
        if current.is_none() {
            sqlx::query(&format!(
                "INSERT INTO {} ({},comment_id,pinned_by) VALUES (?1,?2,?3)",
                s.pins, s.foreign
            ))
            .bind(s.id)
            .bind(comment)
            .bind(actor)
            .execute(&mut *tx)
            .await?;
        }
    } else {
        anyhow::ensure!(current == Some(comment), PinRefusal::Missing);
        sqlx::query(&format!(
            "DELETE FROM {} WHERE {}=?1 AND comment_id=?2",
            s.pins, s.foreign
        ))
        .bind(s.id)
        .bind(comment)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}
