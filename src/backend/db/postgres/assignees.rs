use crate::backend::db::assignees::{AssignedItem, AssignmentRefusal, MAX_ASSIGNEES};
use anyhow::Result;
use sqlx::{PgPool, Postgres, Transaction};

/// The table of the item, its assignee link table and the link column that names the item.
fn tables(item: AssignedItem) -> (&'static str, &'static str, &'static str) {
    match item {
        AssignedItem::Issue(_) => ("issues", "issue_assignees", "issue_id"),
        AssignedItem::PullRequest(_) => ("pull_requests", "pr_assignees", "pull_request_id"),
    }
}

fn item_id(item: AssignedItem) -> i64 {
    match item {
        AssignedItem::Issue(id) | AssignedItem::PullRequest(id) => id,
    }
}

async fn begin_write(
    pool: &PgPool,
    actor_id: i64,
    repo_id: i64,
) -> Result<Transaction<'_, Postgres>> {
    let mut tx = pool.begin().await?;
    anyhow::ensure!(
        super::discussions::lock_write_authority(&mut tx, actor_id, repo_id).await?,
        AssignmentRefusal::Denied
    );
    Ok(tx)
}

/// Checks that the item is in the repository, and returns the link table and column.
async fn locate_item(
    tx: &mut Transaction<'_, Postgres>,
    repo_id: i64,
    item: AssignedItem,
) -> Result<(&'static str, &'static str)> {
    let (table, link, column) = tables(item);
    let found: Option<i64> = sqlx::query_scalar(&format!(
        "SELECT id FROM {table} WHERE id = $1 AND repo_id = $2"
    ))
    .bind(item_id(item))
    .bind(repo_id)
    .fetch_optional(&mut **tx)
    .await?;
    anyhow::ensure!(found.is_some(), AssignmentRefusal::NotFound);
    Ok((link, column))
}

pub async fn assign_user(
    pool: &PgPool,
    actor_id: i64,
    repo_id: i64,
    item: AssignedItem,
    user_id: i64,
) -> Result<()> {
    let mut tx = begin_write(pool, actor_id, repo_id).await?;
    let (link, column) = locate_item(&mut tx, repo_id, item).await?;
    // The personal owner and anyone with direct access to the repository may be assigned.
    let assignable: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM repositories WHERE id = $1 AND owner_user_id = $2)
            OR EXISTS (SELECT 1 FROM repo_access WHERE repo_id = $1 AND user_id = $2 AND access_level >= 1)",
    )
    .bind(repo_id)
    .bind(user_id)
    .fetch_one(&mut *tx)
    .await?;
    anyhow::ensure!(assignable, AssignmentRefusal::Unassignable);
    let assigned: bool = sqlx::query_scalar(&format!(
        "SELECT EXISTS (SELECT 1 FROM {link} WHERE {column} = $1 AND user_id = $2)"
    ))
    .bind(item_id(item))
    .bind(user_id)
    .fetch_one(&mut *tx)
    .await?;
    if !assigned {
        let count: i64 =
            sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {link} WHERE {column} = $1"))
                .bind(item_id(item))
                .fetch_one(&mut *tx)
                .await?;
        anyhow::ensure!(count < MAX_ASSIGNEES, AssignmentRefusal::TooMany);
        sqlx::query(&format!(
            "INSERT INTO {link} ({column}, user_id) VALUES ($1, $2) ON CONFLICT DO NOTHING"
        ))
        .bind(item_id(item))
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

pub async fn unassign_user(
    pool: &PgPool,
    actor_id: i64,
    repo_id: i64,
    item: AssignedItem,
    user_id: i64,
) -> Result<bool> {
    let mut tx = begin_write(pool, actor_id, repo_id).await?;
    let (link, column) = locate_item(&mut tx, repo_id, item).await?;
    let result = sqlx::query(&format!(
        "DELETE FROM {link} WHERE {column} = $1 AND user_id = $2"
    ))
    .bind(item_id(item))
    .bind(user_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(result.rows_affected() > 0)
}

pub async fn list_assignees(pool: &PgPool, item: AssignedItem) -> Result<Vec<String>> {
    let (_, link, column) = tables(item);
    Ok(sqlx::query_scalar(&format!(
        "SELECT u.username FROM {link} a JOIN users u ON u.id = a.user_id WHERE a.{column} = $1 ORDER BY u.username"
    ))
    .bind(item_id(item))
    .fetch_all(pool)
    .await?)
}

/// The assignees of many issues, or of many pull requests, in one query. Each row is an item id
/// and a username, in name order.
pub async fn list_item_assignees(
    pool: &PgPool,
    pull_requests: bool,
    item_ids: &[i64],
) -> Result<Vec<(i64, String)>> {
    if item_ids.is_empty() {
        return Ok(Vec::new());
    }
    debug_assert!(item_ids.len() <= 500);
    let (link, column) = if pull_requests {
        ("pr_assignees", "pull_request_id")
    } else {
        ("issue_assignees", "issue_id")
    };
    let mut query = sqlx::QueryBuilder::<Postgres>::new(format!(
        "SELECT a.{column}, u.username FROM {link} a JOIN users u ON u.id = a.user_id WHERE a.{column} IN ("
    ));
    let mut ids = query.separated(",");
    for id in item_ids {
        ids.push_bind(id);
    }
    query.push(") ORDER BY u.username");
    Ok(query.build_query_as().fetch_all(pool).await?)
}

pub async fn list_assignable_usernames(pool: &PgPool, repo_id: i64) -> Result<Vec<String>> {
    Ok(sqlx::query_scalar(
        "SELECT username FROM users WHERE id IN (
            SELECT owner_user_id FROM repositories WHERE id = $1 AND owner_user_id IS NOT NULL
            UNION SELECT user_id FROM repo_access WHERE repo_id = $1 AND access_level >= 1
        ) ORDER BY username",
    )
    .bind(repo_id)
    .fetch_all(pool)
    .await?)
}
