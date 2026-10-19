use crate::backend::db::models::{WikiPage, WikiPageSummary, WikiRevision};
use crate::backend::db::wiki::{like_pattern, WikiContent, WikiRefusal, PAGE_LIMIT};
use anyhow::Result;
use sqlx::{PgPool, Postgres, Transaction};

const HEAD_SELECT: &str = r#"SELECT p.id, p.repo_id, p.slug, p.created_at, r.page_id, r.number, r.title, r.body,
    r.message, r.author_id, u.username AS author, r.created_at AS saved_at
    FROM wiki_pages p
    JOIN wiki_revisions r ON r.page_id = p.id AND r.number = p.revision_count
    JOIN users u ON u.id = r.author_id"#;

/// Writes lock the repository row first, so the checks and the change see one writer at a time.
/// Returns the transaction and whether the wiki is on. An archived repository refuses every change.
async fn begin_write(
    pool: &PgPool,
    actor_id: i64,
    repo_id: i64,
    admin: bool,
) -> Result<(Transaction<'_, Postgres>, bool)> {
    let mut tx = pool.begin().await?;
    let allowed = if admin {
        super::discussions::lock_repository_mutation_authority(&mut tx, actor_id, repo_id).await?
    } else {
        super::discussions::lock_exclusive_write_authority(&mut tx, actor_id, repo_id).await?
    };
    anyhow::ensure!(allowed, WikiRefusal::Denied);
    let (archived, enabled): (bool, bool) =
        sqlx::query_as("SELECT is_archived, wiki_enabled FROM repositories WHERE id = $1")
            .bind(repo_id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(WikiRefusal::Denied)?;
    anyhow::ensure!(!archived, WikiRefusal::Denied);
    Ok((tx, enabled))
}

async fn head(tx: &mut Transaction<'_, Postgres>, page_id: i64) -> Result<WikiPage> {
    sqlx::query_as::<_, WikiPage>(&format!("{HEAD_SELECT} WHERE p.id = $1"))
        .bind(page_id)
        .fetch_one(&mut **tx)
        .await
        .map_err(Into::into)
}

async fn insert_revision(
    tx: &mut Transaction<'_, Postgres>,
    page_id: i64,
    number: i64,
    actor_id: i64,
    content: &WikiContent<'_>,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO wiki_revisions (page_id, number, title, body, message, author_id) VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(page_id)
    .bind(number)
    .bind(content.title)
    .bind(content.body)
    .bind(content.message)
    .bind(actor_id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

pub async fn create_page(
    pool: &PgPool,
    actor_id: i64,
    repo_id: i64,
    content: &WikiContent<'_>,
) -> Result<WikiPage> {
    let (mut tx, enabled) = begin_write(pool, actor_id, repo_id, false).await?;
    anyhow::ensure!(enabled, WikiRefusal::Denied);
    let pages: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM wiki_pages WHERE repo_id = $1")
        .bind(repo_id)
        .fetch_one(&mut *tx)
        .await?;
    anyhow::ensure!(pages < PAGE_LIMIT, WikiRefusal::PageLimit);
    let taken: Option<i64> =
        sqlx::query_scalar("SELECT id FROM wiki_pages WHERE repo_id = $1 AND slug = $2")
            .bind(repo_id)
            .bind(content.slug)
            .fetch_optional(&mut *tx)
            .await?;
    anyhow::ensure!(taken.is_none(), WikiRefusal::SlugTaken);
    // A new page takes over the address a renamed page left behind.
    sqlx::query("DELETE FROM wiki_redirects WHERE repo_id = $1 AND old_slug = $2")
        .bind(repo_id)
        .bind(content.slug)
        .execute(&mut *tx)
        .await?;
    let page_id: i64 = sqlx::query_scalar(
        "INSERT INTO wiki_pages (repo_id, slug, revision_count) VALUES ($1, $2, 1) RETURNING id",
    )
    .bind(repo_id)
    .bind(content.slug)
    .fetch_one(&mut *tx)
    .await?;
    insert_revision(&mut tx, page_id, 1, actor_id, content).await?;
    let page = head(&mut tx, page_id).await?;
    tx.commit().await?;
    Ok(page)
}

pub async fn edit_page(
    pool: &PgPool,
    actor_id: i64,
    repo_id: i64,
    page_id: i64,
    expected_revision: i64,
    content: &WikiContent<'_>,
) -> Result<WikiPage> {
    let (mut tx, enabled) = begin_write(pool, actor_id, repo_id, false).await?;
    anyhow::ensure!(enabled, WikiRefusal::Denied);
    let (old_slug, number): (String, i64) = sqlx::query_as(
        "SELECT slug, revision_count FROM wiki_pages WHERE id = $1 AND repo_id = $2",
    )
    .bind(page_id)
    .bind(repo_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(WikiRefusal::NotFound)?;
    anyhow::ensure!(number == expected_revision, WikiRefusal::Stale);
    let current = head(&mut tx, page_id).await?;
    if content.slug == old_slug
        && content.title == current.head.title
        && content.body == current.head.body
    {
        return Ok(current);
    }
    if content.slug != old_slug {
        let taken: Option<i64> =
            sqlx::query_scalar("SELECT id FROM wiki_pages WHERE repo_id = $1 AND slug = $2")
                .bind(repo_id)
                .bind(content.slug)
                .fetch_optional(&mut *tx)
                .await?;
        anyhow::ensure!(taken.is_none(), WikiRefusal::SlugTaken);
        // The new title's address leaves the redirects; the old address starts one.
        sqlx::query("DELETE FROM wiki_redirects WHERE repo_id = $1 AND old_slug = $2")
            .bind(repo_id)
            .bind(content.slug)
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "INSERT INTO wiki_redirects (repo_id, old_slug, page_id) VALUES ($1, $2, $3)
             ON CONFLICT (repo_id, old_slug) DO UPDATE SET page_id = EXCLUDED.page_id",
        )
        .bind(repo_id)
        .bind(&old_slug)
        .bind(page_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query("UPDATE wiki_pages SET slug = $1 WHERE id = $2")
            .bind(content.slug)
            .bind(page_id)
            .execute(&mut *tx)
            .await?;
    }
    insert_revision(&mut tx, page_id, number + 1, actor_id, content).await?;
    sqlx::query("UPDATE wiki_pages SET revision_count = $1 WHERE id = $2")
        .bind(number + 1)
        .bind(page_id)
        .execute(&mut *tx)
        .await?;
    let page = head(&mut tx, page_id).await?;
    tx.commit().await?;
    Ok(page)
}

pub async fn delete_page(pool: &PgPool, actor_id: i64, repo_id: i64, page_id: i64) -> Result<()> {
    let (mut tx, enabled) = begin_write(pool, actor_id, repo_id, true).await?;
    anyhow::ensure!(enabled, WikiRefusal::Denied);
    let removed = sqlx::query("DELETE FROM wiki_pages WHERE id = $1 AND repo_id = $2")
        .bind(page_id)
        .bind(repo_id)
        .execute(&mut *tx)
        .await?;
    anyhow::ensure!(removed.rows_affected() == 1, WikiRefusal::NotFound);
    tx.commit().await?;
    Ok(())
}

pub async fn set_wiki_enabled(
    pool: &PgPool,
    actor_id: i64,
    repo_id: i64,
    enabled: bool,
) -> Result<()> {
    let (mut tx, _) = begin_write(pool, actor_id, repo_id, true).await?;
    sqlx::query("UPDATE repositories SET wiki_enabled = $1 WHERE id = $2")
        .bind(enabled)
        .bind(repo_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn get_page_by_slug(pool: &PgPool, repo_id: i64, slug: &str) -> Result<Option<WikiPage>> {
    Ok(sqlx::query_as::<_, WikiPage>(&format!(
        "{HEAD_SELECT} WHERE p.repo_id = $1 AND p.slug = $2"
    ))
    .bind(repo_id)
    .bind(slug)
    .fetch_optional(pool)
    .await?)
}

pub async fn redirect_target(
    pool: &PgPool,
    repo_id: i64,
    old_slug: &str,
) -> Result<Option<String>> {
    Ok(sqlx::query_scalar(
        "SELECT p.slug FROM wiki_redirects r JOIN wiki_pages p ON p.id = r.page_id
         WHERE r.repo_id = $1 AND r.old_slug = $2",
    )
    .bind(repo_id)
    .bind(old_slug)
    .fetch_optional(pool)
    .await?)
}

pub async fn list_pages(
    pool: &PgPool,
    repo_id: i64,
    query: Option<&str>,
) -> Result<Vec<WikiPageSummary>> {
    let pattern = query.map(like_pattern);
    Ok(sqlx::query_as::<_, WikiPageSummary>(
        r#"SELECT p.id, p.slug, r.title, r.created_at AS updated_at
        FROM wiki_pages p
        JOIN wiki_revisions r ON r.page_id = p.id AND r.number = p.revision_count
        WHERE p.repo_id = $1 AND ($2::TEXT IS NULL OR lower(r.title) LIKE $2 ESCAPE '\' OR p.slug LIKE $2 ESCAPE '\')
        ORDER BY lower(r.title), p.slug"#,
    )
    .bind(repo_id)
    .bind(pattern)
    .fetch_all(pool)
    .await?)
}

pub async fn list_revisions(pool: &PgPool, page_id: i64) -> Result<Vec<WikiRevision>> {
    Ok(sqlx::query_as::<_, WikiRevision>(
        r#"SELECT r.page_id, r.number, r.title, ''::TEXT AS body, r.message, r.author_id, u.username AS author,
        r.created_at AS saved_at
        FROM wiki_revisions r JOIN users u ON u.id = r.author_id
        WHERE r.page_id = $1 ORDER BY r.number DESC"#,
    )
    .bind(page_id)
    .fetch_all(pool)
    .await?)
}

pub async fn get_revision(
    pool: &PgPool,
    page_id: i64,
    number: i64,
) -> Result<Option<WikiRevision>> {
    Ok(sqlx::query_as::<_, WikiRevision>(
        r#"SELECT r.page_id, r.number, r.title, r.body, r.message, r.author_id, u.username AS author,
        r.created_at AS saved_at
        FROM wiki_revisions r JOIN users u ON u.id = r.author_id
        WHERE r.page_id = $1 AND r.number = $2"#,
    )
    .bind(page_id)
    .bind(number)
    .fetch_optional(pool)
    .await?)
}
