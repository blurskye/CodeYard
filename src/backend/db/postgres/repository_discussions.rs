use crate::backend::db::repository_discussions::{
    valid_body, Category, Comment, Discussion, Filter, Refusal, Write,
};
use anyhow::Result;
use sqlx::PgPool;

pub async fn enabled(pool: &PgPool, repo: i64) -> Result<bool> {
    Ok(sqlx::query_scalar("SELECT COALESCE((SELECT enabled FROM repository_discussion_settings WHERE repo_id=$1),false)").bind(repo).fetch_one(pool).await?)
}
pub async fn categories(pool: &PgPool, repo: i64) -> Result<Vec<Category>> {
    let mut tx = pool.begin().await?;
    for (name, slug, emoji, description, answerable) in [
        (
            "Announcements",
            "announcements",
            "📣",
            "Updates from maintainers",
            false,
        ),
        (
            "General",
            "general",
            "💬",
            "Chat about the repository",
            false,
        ),
        (
            "Ideas",
            "ideas",
            "💡",
            "Share ideas for new features",
            false,
        ),
        ("Polls", "polls", "📊", "Ask the community", false),
        ("Q&A", "q-a", "🙏", "Ask questions and find answers", true),
        (
            "Show and tell",
            "show-and-tell",
            "🙌",
            "Show something you made",
            false,
        ),
    ] {
        sqlx::query("INSERT INTO discussion_categories(repo_id,name,slug,emoji,description,answerable) VALUES ($1,$2,$3,$4,$5,$6) ON CONFLICT(repo_id,slug) DO NOTHING").bind(repo).bind(name).bind(slug).bind(emoji).bind(description).bind(answerable).execute(&mut *tx).await?;
    }
    let rows = sqlx::query_as("SELECT * FROM discussion_categories WHERE repo_id=$1 ORDER BY id")
        .bind(repo)
        .fetch_all(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(rows)
}
pub async fn category_by_id(pool: &PgPool, id: i64) -> Result<Option<Category>> {
    Ok(
        sqlx::query_as("SELECT * FROM discussion_categories WHERE id=$1")
            .bind(id)
            .fetch_optional(pool)
            .await?,
    )
}
pub async fn get(pool: &PgPool, id: i64) -> Result<Option<Discussion>> {
    Ok(sqlx::query_as("SELECT * FROM discussions WHERE id=$1")
        .bind(id)
        .fetch_optional(pool)
        .await?)
}
pub async fn by_number(pool: &PgPool, repo: i64, number: i32) -> Result<Option<Discussion>> {
    Ok(
        sqlx::query_as("SELECT * FROM discussions WHERE repo_id=$1 AND number=$2")
            .bind(repo)
            .bind(number)
            .fetch_optional(pool)
            .await?,
    )
}
pub async fn comment(pool: &PgPool, id: i64) -> Result<Option<Comment>> {
    Ok(
        sqlx::query_as("SELECT * FROM discussion_comments WHERE id=$1")
            .bind(id)
            .fetch_optional(pool)
            .await?,
    )
}
pub async fn comments(
    pool: &PgPool,
    discussion: i64,
    reply: Option<i64>,
    offset: i64,
    limit: i64,
) -> Result<(i64, Vec<Comment>)> {
    let count=sqlx::query_scalar("SELECT COUNT(*) FROM discussion_comments WHERE discussion_id=$1 AND (reply_to_id=$2 OR (reply_to_id IS NULL AND $2 IS NULL))").bind(discussion).bind(reply).fetch_one(pool).await?;
    let rows=sqlx::query_as("SELECT * FROM discussion_comments WHERE discussion_id=$1 AND (reply_to_id=$2 OR (reply_to_id IS NULL AND $2 IS NULL)) ORDER BY created_at,id LIMIT $3 OFFSET $4").bind(discussion).bind(reply).bind(limit).bind(offset).fetch_all(pool).await?;
    Ok((count, rows))
}
pub async fn labels(
    pool: &PgPool,
    discussion: i64,
) -> Result<Vec<crate::backend::db::models::Label>> {
    Ok(sqlx::query_as("SELECT l.* FROM labels l JOIN discussion_labels a ON a.label_id=l.id WHERE a.discussion_id=$1 ORDER BY l.name,l.id").bind(discussion).fetch_all(pool).await?)
}
pub async fn list(
    pool: &PgPool,
    viewer: i64,
    filter: &Filter,
    offset: i64,
    limit: i64,
) -> Result<(i64, Vec<Discussion>)> {
    let statement = filter.statement(true, &super::repositories::access::readable_condition())?;
    let mut connection = super::repositories::discovery::search_connection(pool).await?;
    let count_sql = format!("SELECT COUNT(*) {}", statement.prefix);
    let mut count = sqlx::query_scalar::<_, i64>(&count_sql).bind(viewer);
    for value in &statement.args {
        count = count.bind(value);
    }
    let total = count.fetch_one(&mut *connection).await?;
    let sql = format!(
        "SELECT i.* {} ORDER BY {} LIMIT {} OFFSET {}",
        statement.prefix,
        filter.order(),
        statement.placeholder(statement.args.len() + 2),
        statement.placeholder(statement.args.len() + 3)
    );
    let mut rows = sqlx::query_as(&sql).bind(viewer);
    for value in &statement.args {
        rows = rows.bind(value);
    }
    let rows = rows
        .bind(limit)
        .bind(offset)
        .fetch_all(&mut *connection)
        .await?;
    connection.commit().await?;
    Ok((total, rows))
}

pub async fn write(
    pool: &PgPool,
    actor: i64,
    repo: i64,
    discussion: Option<i64>,
    change: &Write<'_>,
) -> Result<i64> {
    let mut tx = pool.begin().await?;
    if let Write::Enabled(value) = change {
        anyhow::ensure!(
            super::discussions::lock_repository_mutation_authority(&mut tx, actor, repo).await?,
            Refusal::Denied
        );
        super::repositories::records::features_on(&mut tx, repo, None, Some(*value)).await?;
        tx.commit().await?;
        return Ok(repo);
    }
    anyhow::ensure!(
        super::discussions::lock_contribution_authority(&mut tx, actor, repo).await?,
        Refusal::Denied
    );
    let enabled:bool=sqlx::query_scalar("SELECT COALESCE((SELECT enabled FROM repository_discussion_settings WHERE repo_id=$1),false)").bind(repo).fetch_one(&mut *tx).await?;
    anyhow::ensure!(enabled, Refusal::Denied);
    let current: Option<Discussion> = if let Some(id) = discussion {
        Some(
            sqlx::query_as("SELECT * FROM discussions WHERE id=$1 AND repo_id=$2 FOR UPDATE")
                .bind(id)
                .bind(repo)
                .fetch_optional(&mut *tx)
                .await?
                .ok_or(Refusal::Missing)?,
        )
    } else {
        None
    };
    let id = current.as_ref().map_or(0, |d| d.id);
    let result = match change {
        Write::Create {
            category,
            title,
            body,
        } => {
            anyhow::ensure!(
                super::discussions::lock_write_authority(&mut tx, actor, repo).await?,
                Refusal::Denied
            );
            anyhow::ensure!(
                crate::backend::db::projects::valid_name(title),
                Refusal::Invalid
            );
            valid_body(body)?;
            let valid: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM discussion_categories WHERE id=$1 AND repo_id=$2)",
            )
            .bind(category)
            .bind(repo)
            .fetch_one(&mut *tx)
            .await?;
            anyhow::ensure!(valid, Refusal::Invalid);
            let number = sqlx::query_scalar::<_, i32>("SELECT get_next_issue_number($1)")
                .bind(repo)
                .fetch_one(&mut *tx)
                .await?;
            sqlx::query_scalar("INSERT INTO discussions(repo_id,number,title,body,author_id,category_id) VALUES ($1,$2,$3,$4,$5,$6) RETURNING id").bind(repo).bind(number).bind(title).bind(body).bind(actor).bind(category).fetch_one(&mut *tx).await?
        }
        Write::Edit {
            title,
            body,
            category,
        } => {
            let current = current.as_ref().ok_or(Refusal::Missing)?;
            anyhow::ensure!(
                actor == current.author_id
                    || super::discussions::lock_triage_authority(&mut tx, actor, repo).await?,
                Refusal::Denied
            );
            if let Some(title) = title {
                anyhow::ensure!(
                    crate::backend::db::projects::valid_name(title),
                    Refusal::Invalid
                );
            }
            if let Some(body) = body {
                valid_body(body)?;
            }
            if let Some(category) = category {
                let valid: bool = sqlx::query_scalar(
                    "SELECT EXISTS(SELECT 1 FROM discussion_categories WHERE id=$1 AND repo_id=$2)",
                )
                .bind(category)
                .bind(repo)
                .fetch_one(&mut *tx)
                .await?;
                anyhow::ensure!(valid, Refusal::Invalid);
                // Changing out of an answerable category must not retain an answer.
                let answerable: bool =
                    sqlx::query_scalar("SELECT answerable FROM discussion_categories WHERE id=$1")
                        .bind(category)
                        .fetch_one(&mut *tx)
                        .await?;
                if !answerable {
                    clear_answer(&mut tx, id).await?;
                }
            }
            sqlx::query("UPDATE discussions SET title=COALESCE($2,title),body=COALESCE($3,body),category_id=COALESCE($4,category_id),updated_at=CURRENT_TIMESTAMP WHERE id=$1").bind(id).bind(title).bind(body).bind(category).execute(&mut *tx).await?;
            id
        }
        Write::Delete => {
            let current = current.as_ref().ok_or(Refusal::Missing)?;
            anyhow::ensure!(
                actor == current.author_id
                    || super::discussions::lock_triage_authority(&mut tx, actor, repo).await?,
                Refusal::Denied
            );
            sqlx::query("DELETE FROM discussions WHERE id=$1")
                .bind(id)
                .execute(&mut *tx)
                .await?;
            id
        }
        Write::AddComment { body, reply } => {
            let current = current.as_ref().ok_or(Refusal::Missing)?;
            anyhow::ensure!(
                !current.locked
                    && super::discussions::lock_write_authority(&mut tx, actor, repo).await?,
                Refusal::Denied
            );
            valid_body(body)?;
            anyhow::ensure!(!body.trim().is_empty(), Refusal::Invalid);
            if let Some(reply) = reply {
                let valid:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM discussion_comments WHERE id=$1 AND discussion_id=$2 AND reply_to_id IS NULL)").bind(reply).bind(id).fetch_one(&mut *tx).await?;
                anyhow::ensure!(valid, Refusal::Invalid);
            }
            sqlx::query_scalar("INSERT INTO discussion_comments(discussion_id,author_id,body,reply_to_id) VALUES ($1,$2,$3,$4) RETURNING id").bind(id).bind(actor).bind(body).bind(reply).fetch_one(&mut *tx).await?
        }
        Write::EditComment { comment, .. }
        | Write::DeleteComment(comment)
        | Write::Answer { comment, .. } => {
            let current = current.as_ref().ok_or(Refusal::Missing)?;
            let saved: Comment = sqlx::query_as(
                "SELECT * FROM discussion_comments WHERE id=$1 AND discussion_id=$2",
            )
            .bind(comment)
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(Refusal::Missing)?;
            if let Write::Answer { mark, .. } = change {
                // The discussion's author chooses its answer, as maintainers can.
                anyhow::ensure!(
                    actor == current.author_id
                        || super::discussions::lock_maintain_authority(&mut tx, actor, repo)
                            .await?,
                    Refusal::Denied
                );
                if *mark {
                    let answerable: bool = sqlx::query_scalar(
                        "SELECT answerable FROM discussion_categories WHERE id=$1",
                    )
                    .bind(current.category_id)
                    .fetch_one(&mut *tx)
                    .await?;
                    anyhow::ensure!(answerable, Refusal::Invalid);
                    clear_answer(&mut tx, id).await?;
                    sqlx::query("UPDATE discussions SET answer_id=$2,answer_chosen_at=CURRENT_TIMESTAMP,answer_chosen_by=$3 WHERE id=$1").bind(id).bind(comment).bind(actor).execute(&mut *tx).await?;
                    sqlx::query("UPDATE discussion_comments SET is_answer=true WHERE id=$1")
                        .bind(comment)
                        .execute(&mut *tx)
                        .await?;
                } else {
                    anyhow::ensure!(current.answer_id == Some(*comment), Refusal::Invalid);
                    clear_answer(&mut tx, id).await?;
                }
            } else {
                anyhow::ensure!(
                    actor == saved.author_id
                        || super::discussions::lock_triage_authority(&mut tx, actor, repo).await?,
                    Refusal::Denied
                );
                if let Write::EditComment { body, .. } = change {
                    anyhow::ensure!(!current.locked, Refusal::Denied);
                    valid_body(body)?;
                    anyhow::ensure!(!body.trim().is_empty(), Refusal::Invalid);
                    sqlx::query("UPDATE discussion_comments SET body=$2,updated_at=CURRENT_TIMESTAMP WHERE id=$1").bind(comment).bind(body).execute(&mut *tx).await?;
                } else {
                    let removes_answer:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM discussion_comments WHERE discussion_id=$1 AND is_answer AND (id=$2 OR reply_to_id=$2))").bind(id).bind(comment).fetch_one(&mut *tx).await?;
                    if removes_answer {
                        clear_answer(&mut tx, id).await?;
                    }
                    sqlx::query("DELETE FROM discussion_comments WHERE id=$1")
                        .bind(comment)
                        .execute(&mut *tx)
                        .await?;
                }
            }
            *comment
        }
        Write::Labels { labels, add } => {
            anyhow::ensure!(current.is_some(), Refusal::Missing);
            anyhow::ensure!(
                super::discussions::lock_triage_authority(&mut tx, actor, repo).await?,
                Refusal::Denied
            );
            anyhow::ensure!(labels.len() <= 100, Refusal::Invalid);
            for label in *labels {
                let valid: bool = sqlx::query_scalar(
                    "SELECT EXISTS(SELECT 1 FROM labels WHERE id=$1 AND repo_id=$2)",
                )
                .bind(label)
                .bind(repo)
                .fetch_one(&mut *tx)
                .await?;
                anyhow::ensure!(valid, Refusal::Invalid);
                if *add {
                    sqlx::query("INSERT INTO discussion_labels(discussion_id,label_id) VALUES ($1,$2) ON CONFLICT DO NOTHING").bind(id).bind(label).execute(&mut *tx).await?;
                } else {
                    sqlx::query(
                        "DELETE FROM discussion_labels WHERE discussion_id=$1 AND label_id=$2",
                    )
                    .bind(id)
                    .bind(label)
                    .execute(&mut *tx)
                    .await?;
                }
            }
            id
        }
        Write::Enabled(_) => unreachable!(),
    };
    if id != 0 && !matches!(change, Write::Delete) {
        sqlx::query("UPDATE discussions SET updated_at=CURRENT_TIMESTAMP WHERE id=$1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(result)
}
async fn clear_answer(tx: &mut sqlx::Transaction<'_, sqlx::Postgres>, id: i64) -> Result<()> {
    sqlx::query("UPDATE discussions SET answer_id=NULL,answer_chosen_at=NULL,answer_chosen_by=NULL WHERE id=$1").bind(id).execute(&mut **tx).await?;
    sqlx::query(
        "UPDATE discussion_comments SET is_answer=false WHERE discussion_id=$1 AND is_answer",
    )
    .bind(id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}
