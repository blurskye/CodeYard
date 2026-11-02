use crate::backend::db::reactions::{Reaction, ReactionCount, ReactionRefusal, Subject, Target};
use anyhow::Result;
use sqlx::{Sqlite, SqlitePool, Transaction};

/// Adds the reaction, or removes it when the user already gave it. Returns whether it is now set.
/// BEGIN IMMEDIATE holds the write lock, so the checks and the change see the same state: a revoked
/// grant, an archived repository or a pending comment refuses the write.
#[expect(
    clippy::too_many_arguments,
    reason = "Each argument is one ID, subject or check of the reaction."
)]
pub async fn toggle_reaction(
    pool: &SqlitePool,
    actor_id: i64,
    credential: &crate::backend::db::users::accounts::AccountCredential<'_>,
    repo_id: i64,
    item_id: i64,
    subject: Subject,
    subject_id: i64,
    content: &str,
) -> Result<bool> {
    anyhow::ensure!(
        crate::backend::db::reactions::valid_content(subject, content),
        "Unknown reaction"
    );
    let mut tx = begin_write(
        pool,
        actor_id,
        credential,
        Target {
            repo_id,
            item_id,
            subject,
            subject_id,
        },
    )
    .await?;
    let removed = sqlx::query(
        "DELETE FROM reactions WHERE user_id = ?1 AND subject_kind = ?2 AND subject_id = ?3 AND content = ?4",
    )
    .bind(actor_id)
    .bind(subject.name())
    .bind(subject_id)
    .bind(content)
    .execute(&mut *tx)
    .await?;
    let set = removed.rows_affected() == 0;
    if set {
        sqlx::query(
            "INSERT INTO reactions (repo_id, user_id, subject_kind, subject_id, content) VALUES (?1, ?2, ?3, ?4, ?5) ON CONFLICT DO NOTHING",
        )
        .bind(repo_id)
        .bind(actor_id)
        .bind(subject.name())
        .bind(subject_id)
        .bind(content)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(set)
}

async fn begin_write<'a>(
    pool: &'a SqlitePool,
    actor_id: i64,
    credential: &crate::backend::db::users::accounts::AccountCredential<'_>,
    target: Target,
) -> Result<Transaction<'a, Sqlite>> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    super::discussions::require_contribution_authority(&mut tx, actor_id, target.repo_id).await?;
    let table = match target.subject {
        Subject::Issue | Subject::IssueComment => "issues",
        Subject::Release => "releases",
        Subject::CommitComment => "commit_comments",
        _ => "pull_requests",
    };
    let item: Option<i64> = sqlx::query_scalar(&format!(
        "SELECT id FROM {table} WHERE id=?1 AND repo_id=?2"
    ))
    .bind(target.item_id)
    .bind(target.repo_id)
    .fetch_optional(&mut *tx)
    .await?;
    anyhow::ensure!(
        item.is_some()
            && is_on_item(&mut tx, target.subject, target.subject_id, target.item_id).await?,
        ReactionRefusal::NotFound
    );
    if target.subject == Subject::Release {
        let draft: bool = sqlx::query_scalar("SELECT is_draft FROM releases WHERE id=?1")
            .bind(target.item_id)
            .fetch_one(&mut *tx)
            .await?;
        anyhow::ensure!(
            !draft
                || super::discussions::lock_write_authority(&mut tx, actor_id, target.repo_id)
                    .await?,
            ReactionRefusal::NotFound
        );
    }
    anyhow::ensure!(
        super::users::transitions::credential_live(&mut tx, actor_id, credential, true).await?,
        super::discussions::AUTHORITY_DENIED
    );
    Ok(tx)
}

pub async fn create_reaction(
    pool: &SqlitePool,
    actor_id: i64,
    credential: &crate::backend::db::users::accounts::AccountCredential<'_>,
    target: Target,
    content: &str,
) -> Result<(Reaction, bool)> {
    anyhow::ensure!(
        crate::backend::db::reactions::valid_content(target.subject, content),
        "Unknown reaction"
    );
    let mut tx = begin_write(pool, actor_id, credential, target).await?;
    let existing: Option<Reaction> = sqlx::query_as("SELECT * FROM reactions WHERE user_id=?1 AND subject_kind=?2 AND subject_id=?3 AND content=?4")
        .bind(actor_id).bind(target.subject.name()).bind(target.subject_id).bind(content).fetch_optional(&mut *tx).await?;
    let result = match existing {
        Some(reaction) => (reaction, false),
        None => (sqlx::query_as("INSERT INTO reactions (repo_id,user_id,subject_kind,subject_id,content) VALUES (?1,?2,?3,?4,?5) RETURNING *")
            .bind(target.repo_id).bind(actor_id).bind(target.subject.name()).bind(target.subject_id).bind(content).fetch_one(&mut *tx).await?, true),
    };
    tx.commit().await?;
    Ok(result)
}

pub async fn delete_reaction(
    pool: &SqlitePool,
    actor_id: i64,
    credential: &crate::backend::db::users::accounts::AccountCredential<'_>,
    target: Target,
    reaction_id: i64,
) -> Result<bool> {
    let mut tx = begin_write(pool, actor_id, credential, target).await?;
    let result = sqlx::query("DELETE FROM reactions WHERE id=?1 AND repo_id=?2 AND user_id=?3 AND subject_kind=?4 AND subject_id=?5")
        .bind(reaction_id).bind(target.repo_id).bind(actor_id).bind(target.subject.name()).bind(target.subject_id).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(result.rows_affected() > 0)
}

pub async fn find_reaction(
    pool: &SqlitePool,
    actor: i64,
    target: Target,
    content: &str,
) -> Result<Option<Reaction>> {
    Ok(sqlx::query_as("SELECT * FROM reactions WHERE repo_id=$1 AND user_id=$2 AND subject_kind=$3 AND subject_id=$4 AND content=$5").bind(target.repo_id).bind(actor).bind(target.subject.name()).bind(target.subject_id).bind(content).fetch_optional(pool).await?)
}

pub async fn list_reactions(
    pool: &SqlitePool,
    target: Target,
    content: Option<&str>,
    limit: i64,
    offset: i64,
) -> Result<Vec<Reaction>> {
    Ok(sqlx::query_as("SELECT * FROM reactions WHERE repo_id=?1 AND subject_kind=?2 AND subject_id=?3 AND (?4 IS NULL OR content=?4) ORDER BY id LIMIT ?5 OFFSET ?6")
        .bind(target.repo_id).bind(target.subject.name()).bind(target.subject_id).bind(content).bind(limit).bind(offset).fetch_all(pool).await?)
}

/// The reactions on `ids`, in the order they were first given.
pub async fn reaction_counts(
    pool: &SqlitePool,
    subject: &str,
    ids: &[i64],
    viewer_id: i64,
) -> Result<Vec<ReactionCount>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let rows: Vec<(i64, String, i64, i64)> = sqlx::query_as(
        r#"
        SELECT subject_id, content, COUNT(*), MAX(user_id = ?3)
        FROM reactions
        WHERE subject_kind = ?1 AND subject_id IN (SELECT value FROM json_each(?2))
        GROUP BY subject_id, content
        ORDER BY MIN(id)
        "#,
    )
    .bind(subject)
    .bind(serde_json::to_string(ids)?)
    .bind(viewer_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(subject_id, content, count, mine)| ReactionCount {
            subject_id,
            content,
            count,
            mine: mine != 0,
        })
        .collect())
}

/// Whether the subject is the item itself, or a comment or review on it. Comments and reviews of a
/// pending review are left out, so nobody can react to them before they are submitted.
async fn is_on_item(
    tx: &mut Transaction<'_, Sqlite>,
    subject: Subject,
    id: i64,
    item_id: i64,
) -> Result<bool> {
    let sql = match subject {
        Subject::Issue | Subject::PullRequest | Subject::Release | Subject::CommitComment => return Ok(id == item_id),
        Subject::IssueComment => "SELECT 1 FROM issue_comments WHERE id = ?1 AND issue_id = ?2",
        Subject::PullRequestComment => "SELECT 1 FROM pr_comments c WHERE c.id = ?1 AND c.pull_request_id = ?2 AND NOT EXISTS (SELECT 1 FROM pr_reviews v WHERE v.id = c.review_id AND v.state = 'pending')",
        Subject::PullRequestReview => "SELECT 1 FROM pr_reviews WHERE id = ?1 AND pull_request_id = ?2 AND state <> 'pending'",
    };
    let found: Option<i32> = sqlx::query_scalar(sql)
        .bind(id)
        .bind(item_id)
        .fetch_optional(&mut **tx)
        .await?;
    Ok(found.is_some())
}
