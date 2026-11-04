use crate::repositories::commit_comments::validate_body;
use crate::repositories::commit_comments::validate_create;
use crate::repositories::commit_comments::CommentAuthor;
use crate::repositories::commit_comments::CommentCreate;
use crate::repositories::commit_comments::CommentEdit;
use crate::repositories::commit_comments::CommentEdited;
use crate::repositories::commit_comments::CommentRefusal;
use anyhow::Result;
use codeyard_db_core::models::CommitComment;
use sqlx::PgPool;

pub async fn create_comment(pool: &PgPool, input: CommentCreate<'_>) -> Result<CommitComment> {
    validate_create(&input)?;
    let mut tx = pool.begin().await?;
    anyhow::ensure!(
        crate::postgres::discussions::lock_contribution_authority(
            &mut tx,
            input.actor.user_id,
            input.repo_id
        )
        .await?,
        CommentRefusal::Denied
    );
    anyhow::ensure!(
        codeyard_db_core::queries::postgres::credentials::writes(
            &mut tx,
            &input.actor,
            Some(input.repo_id),
            None,
            "contents"
        )
        .await?,
        CommentRefusal::Denied
    );
    let comment = sqlx::query_as("INSERT INTO commit_comments(repo_id,author_id,sha,body,path,position,line) VALUES($1,$2,$3,$4,$5,$6,$7) RETURNING *")
        .bind(input.repo_id).bind(input.actor.user_id).bind(input.sha.to_ascii_lowercase()).bind(input.body).bind(input.path).bind(input.position).bind(input.line).fetch_one(&mut *tx).await?;
    tx.commit().await?;
    Ok(comment)
}

pub async fn edit_comment(pool: &PgPool, input: CommentEdit<'_>) -> Result<Option<CommentEdited>> {
    validate_body(input.body)?;
    let mut tx = pool.begin().await?;
    anyhow::ensure!(
        crate::postgres::discussions::lock_contribution_authority(
            &mut tx,
            input.actor.user_id,
            input.repo_id
        )
        .await?,
        CommentRefusal::Denied
    );
    anyhow::ensure!(
        codeyard_db_core::queries::postgres::credentials::writes(
            &mut tx,
            &input.actor,
            Some(input.repo_id),
            None,
            "contents"
        )
        .await?,
        CommentRefusal::Denied
    );
    let Some(previous) = sqlx::query_as::<_, CommitComment>(
        "SELECT * FROM commit_comments WHERE repo_id=$1 AND id=$2 FOR UPDATE",
    )
    .bind(input.repo_id)
    .bind(input.id)
    .fetch_optional(&mut *tx)
    .await?
    else {
        return Ok(None);
    };
    anyhow::ensure!(
        previous.author_id == Some(input.actor.user_id),
        CommentRefusal::Denied
    );
    anyhow::ensure!(
        input.expected_revision.is_none_or(|revision| revision
            == crate::discussions::comment_revision(&previous.body)),
        CommentRefusal::Conflict
    );
    let comment = sqlx::query_as("UPDATE commit_comments SET body=$3,updated_at=clock_timestamp() WHERE repo_id=$1 AND id=$2 RETURNING *")
        .bind(input.repo_id).bind(input.id).bind(input.body).fetch_one(&mut *tx).await?;
    tx.commit().await?;
    Ok(Some(CommentEdited {
        comment,
        previous_body: previous.body,
    }))
}

pub async fn delete_comment(
    pool: &PgPool,
    actor: impl Into<codeyard_db_core::queries::credentials::Actor>,
    repo: i64,
    id: i64,
    expected_revision: Option<&str>,
) -> Result<Option<CommitComment>> {
    let credential = actor.into();
    let actor = credential.user_id;
    let mut tx = pool.begin().await?;
    anyhow::ensure!(
        crate::postgres::discussions::lock_contribution_authority(&mut tx, actor, repo).await?,
        CommentRefusal::Denied
    );
    anyhow::ensure!(
        codeyard_db_core::queries::postgres::credentials::writes(
            &mut tx,
            &credential,
            Some(repo),
            None,
            "contents"
        )
        .await?,
        CommentRefusal::Denied
    );
    let Some(comment) = sqlx::query_as::<_, CommitComment>(
        "SELECT * FROM commit_comments WHERE repo_id=$1 AND id=$2 FOR UPDATE",
    )
    .bind(repo)
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?
    else {
        return Ok(None);
    };
    anyhow::ensure!(
        comment.author_id == Some(actor)
            || crate::postgres::discussions::lock_maintain_authority(&mut tx, actor, repo).await?,
        CommentRefusal::Denied
    );
    anyhow::ensure!(
        expected_revision
            .is_none_or(|revision| revision == crate::discussions::comment_revision(&comment.body)),
        CommentRefusal::Conflict
    );
    sqlx::query("DELETE FROM notifications WHERE repo_id=$1 AND metadata->>'commit_comment_id'=$2")
        .bind(repo)
        .bind(id.to_string())
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM commit_comments WHERE repo_id=$1 AND id=$2")
        .bind(repo)
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Some(comment))
}

pub async fn get_comment(pool: &PgPool, repo: i64, id: i64) -> Result<Option<CommitComment>> {
    Ok(
        sqlx::query_as("SELECT * FROM commit_comments WHERE repo_id=$1 AND id=$2")
            .bind(repo)
            .bind(id)
            .fetch_optional(pool)
            .await?,
    )
}

pub async fn list_comments(
    pool: &PgPool,
    repo: i64,
    sha: Option<&str>,
    offset: i64,
    limit: i64,
) -> Result<Vec<CommitComment>> {
    anyhow::ensure!(
        (0..=1_000_000).contains(&offset) && (0..=100).contains(&limit),
        CommentRefusal::Invalid
    );
    Ok(sqlx::query_as("SELECT * FROM commit_comments WHERE repo_id=$1 AND ($2::text IS NULL OR sha=$2) ORDER BY id LIMIT $3 OFFSET $4").bind(repo).bind(sha).bind(limit).bind(offset).fetch_all(pool).await?)
}

pub async fn comment_counts(
    pool: &PgPool,
    repo: i64,
    shas: &[String],
) -> Result<Vec<(String, i64)>> {
    crate::repositories::commit_comments::validate_counts(shas)?;
    Ok(sqlx::query_as(
        "SELECT sha,COUNT(*) FROM commit_comments WHERE repo_id=$1 AND sha=ANY($2) GROUP BY sha",
    )
    .bind(repo)
    .bind(shas)
    .fetch_all(pool)
    .await?)
}

pub async fn count_comments(
    pool: &PgPool,
    repo: i64,
    sha: &str,
    through: Option<i64>,
) -> Result<i64> {
    Ok(sqlx::query_scalar("SELECT COUNT(*) FROM commit_comments WHERE repo_id=$1 AND sha=$2 AND ($3::bigint IS NULL OR id<=$3)").bind(repo).bind(sha).bind(through).fetch_one(pool).await?)
}

pub async fn comment_authors(pool: &PgPool, repo: i64, ids: &[i64]) -> Result<Vec<CommentAuthor>> {
    anyhow::ensure!(ids.len() <= 100, CommentRefusal::Invalid);
    Ok(sqlx::query_as("SELECT u.id,u.username,u.email,u.is_admin,CASE WHEN r.owner_user_id=u.id THEN 'OWNER' WHEN EXISTS(SELECT 1 FROM org_memberships m WHERE m.org_id=r.owner_org_id AND m.user_id=u.id) THEN 'MEMBER' WHEN EXISTS(SELECT 1 FROM repo_access a WHERE a.repo_id=r.id AND a.user_id=u.id AND a.access_level>0) THEN 'COLLABORATOR' ELSE 'NONE' END AS association FROM users u CROSS JOIN repositories r WHERE r.id=$1 AND u.id=ANY($2)")
        .bind(repo).bind(ids).fetch_all(pool).await?)
}
