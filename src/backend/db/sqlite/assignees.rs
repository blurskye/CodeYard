use crate::backend::db::assignees::{AssignedItem, AssignmentRefusal, MAX_ASSIGNEES};
use anyhow::Result;
use sqlx::{Sqlite, SqlitePool, Transaction};

// Public visibility alone does not make a user an assignment candidate.
const CANDIDATES: &str = "u.id IN (
    SELECT owner_user_id FROM repositories WHERE id=?1 AND owner_user_id IS NOT NULL
    UNION SELECT user_id FROM repo_access WHERE repo_id=?1 AND access_level BETWEEN 1 AND 5
    UNION SELECT m.user_id FROM repositories r JOIN org_memberships m ON m.org_id=r.owner_org_id
        WHERE r.id=?1 AND (m.role IN ('owner','admin') OR (m.role='member' AND EXISTS (
            SELECT 1 FROM teams t JOIN effective_team_memberships tm ON tm.team_id=t.id AND tm.user_id=m.user_id
            JOIN team_repo_access a ON a.team_id=t.id AND a.repo_id=r.id
            WHERE t.org_id=m.org_id AND t.privacy IN ('visible','secret')
                AND tm.role IN ('member','maintainer') AND a.permission BETWEEN 1 AND 5)))
    UNION SELECT m.user_id FROM repositories r JOIN org_memberships m ON m.org_id=r.owner_org_id
        JOIN organization_settings s ON s.org_id=m.org_id AND s.default_repository_permission IN (1,3,5)
        WHERE r.id=?1 AND m.role IN ('owner','admin','member')
    UNION SELECT m.user_id FROM repositories r JOIN org_memberships m ON m.org_id=r.owner_org_id
        JOIN organization_role_assignments a ON a.org_id=m.org_id
        JOIN organization_roles role ON role.id=a.role_id AND role.base_level BETWEEN 1 AND 5
        WHERE r.id=?1 AND m.role IN ('owner','admin','member')
            AND ((a.user_id=m.user_id AND a.team_id IS NULL) OR (a.user_id IS NULL AND EXISTS(
                SELECT 1 FROM teams t JOIN effective_team_memberships tm ON tm.team_id=t.id
                WHERE t.id=a.team_id AND t.org_id=a.org_id AND t.privacy IN ('visible','secret')
                    AND tm.user_id=m.user_id AND tm.role IN ('member','maintainer'))))
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

async fn begin_write<'a>(
    pool: &'a SqlitePool,
    authority: &crate::backend::db::users::accounts::AccountActor,
    repo_id: i64,
) -> Result<Transaction<'a, Sqlite>> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    anyhow::ensure!(
        super::discussions::lock_triage_authority(&mut tx, authority.user_id(), repo_id).await?,
        AssignmentRefusal::Denied
    );
    if let Some(credential) = authority.credential() {
        super::users::transitions::require_account(&mut tx, authority.user_id(), &credential)
            .await.map_err(|error| {
                if error.is::<crate::backend::db::users::accounts::AccountRestricted>() {
                    AssignmentRefusal::Denied.into()
                } else { error }
            })?;
    }
    Ok(tx)
}

async fn record_change(
    tx: &mut Transaction<'_, Sqlite>,
    repo: i64,
    item: AssignedItem,
    actor: i64,
    kind: &str,
    username: &str,
) -> Result<()> {
    let subject = match item { AssignedItem::Issue(_) => "issue", AssignedItem::PullRequest(_) => "pull_request" };
    sqlx::query("INSERT INTO timeline_events(repo_id,subject_kind,subject_id,actor_id,kind,detail) VALUES (?1,?2,?3,?4,?5,?6)")
        .bind(repo).bind(subject).bind(item_id(item)).bind(actor).bind(kind)
        .bind(username.chars().take(255).collect::<String>()).execute(&mut **tx).await?;
    Ok(())
}

/// Checks that the item is in the repository, and returns the link table and column.
async fn locate_item(
    tx: &mut Transaction<'_, Sqlite>,
    repo_id: i64,
    item: AssignedItem,
) -> Result<(&'static str, &'static str)> {
    let (table, link, column) = tables(item);
    let found: Option<i64> = sqlx::query_scalar(&format!(
        "SELECT id FROM {table} WHERE id = ?1 AND repo_id = ?2"
    ))
    .bind(item_id(item))
    .bind(repo_id)
    .fetch_optional(&mut **tx)
    .await?;
    anyhow::ensure!(found.is_some(), AssignmentRefusal::NotFound);
    Ok((link, column))
}

/// Assigns each of `user_ids` in one transaction. Returns the ones that were not assigned before.
pub async fn assign_users(
    pool: &SqlitePool,
    actor_id: impl Into<crate::backend::db::users::accounts::AccountActor>,
    repo_id: i64,
    item: AssignedItem,
    user_ids: &[i64],
) -> Result<Vec<i64>> {
    let authority = actor_id.into();
    let mut tx = begin_write(pool, &authority, repo_id).await?;
    let mut added = Vec::new();
    for &user_id in user_ids {
        if assign_with_event_in_transaction(&mut tx, repo_id, item, authority.user_id(), user_id).await? {
            added.push(user_id);
        }
    }
    tx.commit().await?;
    Ok(added)
}

/// Caller retains live item-metadata authority and the transaction through commit.
pub(crate) async fn candidate(
    tx: &mut Transaction<'_, Sqlite>,
    repo_id: i64,
    user_id: i64,
) -> Result<()> {
    let assignable: bool = sqlx::query_scalar(&format!(
        "SELECT EXISTS (SELECT 1 FROM repositories r JOIN users u ON u.id=?2
         WHERE r.id=?1 AND {CANDIDATES} AND ({})>=1)",
        super::repositories::access::USER_REPO_ACCESS_SQL
    ))
    .bind(repo_id)
    .bind(user_id)
    .fetch_one(&mut **tx)
    .await?;
    anyhow::ensure!(assignable, AssignmentRefusal::Unassignable);
    Ok(())
}
pub(crate) async fn assign_in_transaction(
    tx: &mut Transaction<'_, Sqlite>,
    repo_id: i64,
    item: AssignedItem,
    user_id: i64,
) -> Result<bool> {
    let (link, column) = locate_item(tx, repo_id, item).await?;
    // A user who is already assigned stays assigned, even after losing access, so only a new
    // assignee must be a candidate.
    let assigned: bool = sqlx::query_scalar(&format!(
        "SELECT EXISTS (SELECT 1 FROM {link} WHERE {column} = ?1 AND user_id = ?2)"
    ))
    .bind(item_id(item))
    .bind(user_id)
    .fetch_one(&mut **tx)
    .await?;
    let mut added = false;
    if !assigned {
        candidate(tx, repo_id, user_id).await?;
        let count: i64 =
            sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {link} WHERE {column} = ?1"))
                .bind(item_id(item))
                .fetch_one(&mut **tx)
                .await?;
        anyhow::ensure!(count < MAX_ASSIGNEES, AssignmentRefusal::TooMany);
        // A row that a concurrent request inserted first is not a new assignment.
        added = sqlx::query(&format!(
            "INSERT INTO {link} ({column}, user_id) VALUES (?1, ?2) ON CONFLICT DO NOTHING"
        ))
        .bind(item_id(item))
        .bind(user_id)
        .execute(&mut **tx)
        .await?
        .rows_affected()
            == 1;
    }
    if added {
        super::labels::touch_subject(tx, tables(item).0, item_id(item)).await?;
    }
    Ok(added)
}

pub(crate) async fn assign_with_event_in_transaction(
    tx: &mut Transaction<'_, Sqlite>,
    repo: i64,
    item: AssignedItem,
    actor: i64,
    user: i64,
) -> Result<bool> {
    let added = assign_in_transaction(tx, repo, item, user).await?;
    if added {
        let username: String = sqlx::query_scalar("SELECT username FROM users WHERE id=?1")
            .bind(user).fetch_one(&mut **tx).await?;
        record_change(tx, repo, item, actor, "assigned", &username).await?;
    }
    Ok(added)
}

/// Removes each user named in `usernames` in one transaction. Returns the names that were
/// assigned. A name that is no user, or is not assigned, removes nothing.
pub async fn unassign_users(
    pool: &SqlitePool,
    actor_id: impl Into<crate::backend::db::users::accounts::AccountActor>,
    repo_id: i64,
    item: AssignedItem,
    usernames: &[String],
) -> Result<Vec<String>> {
    let authority = actor_id.into();
    let mut tx = begin_write(pool, &authority, repo_id).await?;
    let (link, column) = locate_item(&mut tx, repo_id, item).await?;
    let mut removed = Vec::new();
    for username in usernames {
        let deleted = sqlx::query(&format!(
            "DELETE FROM {link} WHERE {column} = ?1 AND user_id = (SELECT id FROM users WHERE username = ?2)"
        ))
        .bind(item_id(item))
        .bind(username)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if deleted > 0 {
            record_change(&mut tx, repo_id, item, authority.user_id(), "unassigned", username).await?;
            removed.push(username.clone());
        }
    }
    if !removed.is_empty() {
        super::labels::touch_subject(&mut tx, tables(item).0, item_id(item)).await?;
    }
    tx.commit().await?;
    Ok(removed)
}

pub async fn list_assignees(pool: &SqlitePool, item: AssignedItem) -> Result<Vec<String>> {
    let (_, link, column) = tables(item);
    Ok(sqlx::query_scalar(&format!(
        "SELECT u.username FROM {link} a JOIN users u ON u.id = a.user_id WHERE a.{column} = ?1 ORDER BY u.username"
    ))
    .bind(item_id(item))
    .fetch_all(pool)
    .await?)
}

/// The assignees of many issues, or of many pull requests, in one query. Each row is an item id
/// and a username, in name order.
pub async fn list_item_assignees(
    pool: &SqlitePool,
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
    let mut query = sqlx::QueryBuilder::<Sqlite>::new(format!(
        "SELECT a.{column}, u.username FROM {link} a JOIN users u ON u.id = a.user_id WHERE a.{column} IN ("
    ));
    let mut ids = query.separated(",");
    for id in item_ids {
        ids.push_bind(id);
    }
    query.push(") ORDER BY u.username");
    Ok(query.build_query_as().fetch_all(pool).await?)
}

pub async fn list_assignable_usernames(pool: &SqlitePool, repo_id: i64) -> Result<Vec<String>> {
    Ok(sqlx::query_scalar(&format!(
        "SELECT u.username FROM repositories r JOIN users u ON {CANDIDATES}
         WHERE r.id=?1 AND ({})>=1 ORDER BY u.username",
        super::repositories::access::USER_REPO_ACCESS_SQL
    ))
    .bind(repo_id)
    .fetch_all(pool)
    .await?)
}

/// The users among `user_ids` who may be assigned in the repository.
pub async fn assignable_user_ids(
    pool: &SqlitePool,
    repo_id: i64,
    user_ids: &[i64],
) -> Result<Vec<i64>> {
    if user_ids.is_empty() {
        return Ok(Vec::new());
    }
    Ok(sqlx::query_scalar(&format!(
        "SELECT u.id FROM repositories r JOIN users u ON {CANDIDATES}
         WHERE r.id=?1 AND u.id IN (SELECT value FROM json_each(?2)) AND ({})>=1",
        super::repositories::access::USER_REPO_ACCESS_SQL
    ))
    .bind(repo_id)
    .bind(serde_json::to_string(user_ids)?)
    .fetch_all(pool)
    .await?)
}
