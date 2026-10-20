use crate::backend::db::assignees::{AssignedItem, AssignmentRefusal, MAX_ASSIGNEES};
use anyhow::Result;
use sqlx::{PgPool, Postgres, Transaction};

// Public visibility alone does not make a user an assignment candidate.
const CANDIDATES: &str = "u.id IN (
    SELECT owner_user_id FROM repositories WHERE id=$1 AND owner_user_id IS NOT NULL
    UNION SELECT user_id FROM repo_access WHERE repo_id=$1 AND access_level BETWEEN 1 AND 5
    UNION SELECT m.user_id FROM repositories r JOIN org_memberships m ON m.org_id=r.owner_org_id
        WHERE r.id=$1 AND (m.role IN ('owner','admin') OR (m.role='member' AND EXISTS (
            SELECT 1 FROM teams t JOIN team_memberships tm ON tm.team_id=t.id AND tm.user_id=m.user_id
            JOIN team_repo_access a ON a.team_id=t.id AND a.repo_id=r.id
            WHERE t.org_id=m.org_id AND t.privacy IN ('visible','secret')
                AND tm.role IN ('member','maintainer') AND a.permission BETWEEN 1 AND 5)))
)";

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

/// Checks that the item is in the repository and locks its row, so that writes to one item's
/// assignees run one at a time and each counts the assignees the earlier ones left. Returns the
/// link table and column.
async fn locate_item(
    tx: &mut Transaction<'_, Postgres>,
    repo_id: i64,
    item: AssignedItem,
) -> Result<(&'static str, &'static str)> {
    let (table, link, column) = tables(item);
    let found: Option<i64> = sqlx::query_scalar(&format!(
        "SELECT id FROM {table} WHERE id = $1 AND repo_id = $2 FOR UPDATE"
    ))
    .bind(item_id(item))
    .bind(repo_id)
    .fetch_optional(&mut **tx)
    .await?;
    anyhow::ensure!(found.is_some(), AssignmentRefusal::NotFound);
    Ok((link, column))
}

/// Assigns `user_id`. Returns whether that was a new assignment: an existing assignee returns false.
pub async fn assign_user(
    pool: &PgPool,
    actor_id: i64,
    repo_id: i64,
    item: AssignedItem,
    user_id: i64,
) -> Result<bool> {
    let mut tx = begin_write(pool, actor_id, repo_id).await?;
    let (link, column) = locate_item(&mut tx, repo_id, item).await?;
    let assignable: bool = sqlx::query_scalar(&format!(
        "SELECT EXISTS (SELECT 1 FROM repositories r JOIN users u ON u.id=$2
         WHERE r.id=$1 AND {CANDIDATES} AND ({})>=1)",
        super::repositories::access::USER_REPO_ACCESS_SQL
    ))
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
    let mut added = false;
    if !assigned {
        let count: i64 =
            sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {link} WHERE {column} = $1"))
                .bind(item_id(item))
                .fetch_one(&mut *tx)
                .await?;
        anyhow::ensure!(count < MAX_ASSIGNEES, AssignmentRefusal::TooMany);
        // A row that a concurrent request inserted first is not a new assignment.
        added = sqlx::query(&format!(
            "INSERT INTO {link} ({column}, user_id) VALUES ($1, $2) ON CONFLICT DO NOTHING"
        ))
        .bind(item_id(item))
        .bind(user_id)
        .execute(&mut *tx)
        .await?
        .rows_affected()
            == 1;
    }
    tx.commit().await?;
    Ok(added)
}

pub async fn unassign_user(
    pool: &PgPool,
    actor_id: i64,
    repo_id: i64,
    item: AssignedItem,
    username: &str,
) -> Result<bool> {
    let mut tx = begin_write(pool, actor_id, repo_id).await?;
    let (link, column) = locate_item(&mut tx, repo_id, item).await?;
    // A name that is no user matches no row, so nothing is removed.
    let result = sqlx::query(&format!(
        "DELETE FROM {link} WHERE {column} = $1 AND user_id = (SELECT id FROM users WHERE username = $2)"
    ))
    .bind(item_id(item))
    .bind(username)
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
    Ok(sqlx::query_scalar(&format!(
        "SELECT u.username FROM repositories r JOIN users u ON {CANDIDATES}
         WHERE r.id=$1 AND ({})>=1 ORDER BY u.username",
        super::repositories::access::USER_REPO_ACCESS_SQL
    ))
    .bind(repo_id)
    .fetch_all(pool)
    .await?)
}
