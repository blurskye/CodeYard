use crate::assignees::AssignedItem;
use crate::pins::scope;
use crate::pins::ItemPin;
use crate::pins::Pin;
use crate::pins::PinRefusal;
use anyhow::Result;
use codeyard_db_core::pool::PgPool;

pub async fn get_pin(pool: &PgPool, item: AssignedItem) -> Result<Option<Pin>> {
    let s = scope(item);
    Ok(sqlx::query_as(&format!(
        "SELECT comment_id,pinned_by,pinned_at FROM {} WHERE {}=$1",
        s.pins, s.foreign
    ))
    .bind(s.id)
    .fetch_optional(pool)
    .await?)
}
pub async fn get_pins(pool: &PgPool, pull_requests: bool, ids: &[i64]) -> Result<Vec<ItemPin>> {
    anyhow::ensure!(ids.len() <= 500, "Pin batch exceeds its limit");
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let (pins, foreign) = if pull_requests {
        ("pr_comment_pins", "pull_request_id")
    } else {
        ("issue_comment_pins", "issue_id")
    };
    Ok(sqlx::query_as(&format!(
        "SELECT {foreign} AS item_id,comment_id,pinned_by,pinned_at FROM {pins} WHERE {foreign} IN (SELECT unnest($1::BIGINT[]))"
    ))
    .bind(ids)
    .fetch_all(pool)
    .await?)
}
pub async fn set_pin(
    pool: &PgPool,
    actor: i64,
    credential: &codeyard_db_core::queries::users::accounts::AccountCredential<'_>,
    repo: i64,
    item: AssignedItem,
    comment: i64,
    pinned: bool,
) -> Result<()> {
    let s = scope(item);
    let mut tx = pool.begin().await?;
    let permission = if s.pull { "pull_requests" } else { "issues" };
    anyhow::ensure!(
        codeyard_db_core::queries::postgres::discussions::lock_write_authority(
            &mut tx, actor, repo
        )
        .await?
            && codeyard_db_core::queries::postgres::credentials::repository(
                &mut tx,
                &codeyard_db_core::queries::credentials::Actor::from((actor, credential)),
                repo,
                permission,
                true
            )
            .await?,
        PinRefusal::Denied
    );
    let present: Option<i64> = sqlx::query_scalar(&format!(
        "SELECT id FROM {} WHERE id=$1 AND repo_id=$2 FOR UPDATE",
        s.parent
    ))
    .bind(s.id)
    .bind(repo)
    .fetch_optional(&mut *tx)
    .await?;
    anyhow::ensure!(present.is_some(), PinRefusal::Missing);
    let present: Option<i64> = sqlx::query_scalar(&format!(
        "SELECT id FROM {} WHERE id=$1 AND {}=$2 {} FOR SHARE",
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
    .fetch_optional(&mut *tx)
    .await?;
    anyhow::ensure!(present.is_some(), PinRefusal::Missing);
    let current: Option<i64> = sqlx::query_scalar(&format!(
        "SELECT comment_id FROM {} WHERE {}=$1",
        s.pins, s.foreign
    ))
    .bind(s.id)
    .fetch_optional(&mut *tx)
    .await?;
    if pinned {
        anyhow::ensure!(current.is_none_or(|id| id == comment), PinRefusal::Occupied);
        if current.is_none() {
            sqlx::query(&format!(
                "INSERT INTO {} ({},comment_id,pinned_by) VALUES ($1,$2,$3)",
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
            "DELETE FROM {} WHERE {}=$1 AND comment_id=$2",
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
