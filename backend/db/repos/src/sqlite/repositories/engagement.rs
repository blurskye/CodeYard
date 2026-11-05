use crate::repositories::engagement::StarSort;
use anyhow::Result;
use codeyard_db_core::models::Repository;
use codeyard_db_core::models::Star;
use codeyard_db_core::models::User;
use codeyard_db_core::models::Watch;
use codeyard_db_core::pool::SqlitePool;
use codeyard_db_core::queries::sqlite::repositories::access::lock_readable_repository_in_transaction;
use codeyard_db_core::queries::sqlite::repositories::access::readable_condition;
use codeyard_db_core::queries::sqlite::search::contains;
use codeyard_db_identity::users::blocks::InteractionBlocked;
use sqlx::Sqlite;
use sqlx::Transaction;

/// Checks that the user can still read the repository. The caller's earlier check may be stale
/// by now; BEGIN IMMEDIATE keeps the answer valid until commit.
async fn require_readable(
    tx: &mut Transaction<'_, Sqlite>,
    user_id: i64,
    repo_id: i64,
) -> Result<()> {
    anyhow::ensure!(
        lock_readable_repository_in_transaction(tx, user_id, repo_id).await?,
        InteractionBlocked
    );
    Ok(())
}

/// Removing one's own star or watch needs a live credential, not read access or interaction rights.
async fn begin_removal(
    pool: &SqlitePool,
    actor: codeyard_db_core::queries::credentials::Actor,
) -> Result<(Transaction<'_, Sqlite>, i64)> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    if let Some(credential) = actor.credential() {
        codeyard_db_identity::sqlite::users::transitions::require_account(
            &mut tx,
            actor.user_id,
            &credential,
        )
        .await?;
    }
    Ok((tx, actor.user_id))
}

async fn begin_change(
    pool: &SqlitePool,
    actor: codeyard_db_core::queries::credentials::Actor,
    repo_id: i64,
) -> Result<(Transaction<'_, Sqlite>, i64)> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let user_id = actor.user_id;
    codeyard_db_identity::sqlite::users::blocks::require_repository_interaction(
        &mut tx, user_id, repo_id,
    )
    .await?;
    if let Some(credential) = actor.credential() {
        codeyard_db_identity::sqlite::users::transitions::require_account(
            &mut tx,
            user_id,
            &credential,
        )
        .await?;
    }
    require_readable(&mut tx, user_id, repo_id).await?;
    Ok((tx, user_id))
}

pub async fn list_star_history(
    pool: &SqlitePool,
    repo_id: i64,
    limit: i64,
    offset: i64,
) -> Result<Vec<crate::repositories::engagement::StarHistoryWeek>> {
    let sums=(0..7).map(|day|format!("SUM(CASE WHEN CAST(strftime('%w',day) AS INTEGER)={day} THEN stars ELSE 0 END) AS day{day}")).collect::<Vec<_>>().join(",");
    let sql=format!("SELECT date(day,'-' || strftime('%w',day) || ' days') AS week,{sums} FROM repository_star_days WHERE repo_id=?1 GROUP BY week ORDER BY week DESC LIMIT ?2 OFFSET ?3");
    Ok(sqlx::query_as(&sql)
        .bind(repo_id)
        .bind(limit.clamp(1, 31))
        .bind(offset)
        .fetch_all(pool)
        .await?)
}

pub async fn get_repo_watch(
    pool: &SqlitePool,
    user_id: i64,
    repo_id: i64,
) -> Result<Option<Watch>> {
    Ok(
        sqlx::query_as("SELECT * FROM watches WHERE user_id=?1 AND repo_id=?2")
            .bind(user_id)
            .bind(repo_id)
            .fetch_optional(pool)
            .await?,
    )
}
pub async fn list_repo_watchers(
    pool: &SqlitePool,
    repo_id: i64,
    limit: i64,
    offset: i64,
) -> Result<Vec<User>> {
    Ok(sqlx::query_as("SELECT u.* FROM watches w JOIN users u ON u.id=w.user_id WHERE w.repo_id=?1 AND w.level IN ('watching','custom') ORDER BY w.created_at DESC,w.user_id DESC LIMIT ?2 OFFSET ?3").bind(repo_id).bind(limit).bind(offset).fetch_all(pool).await?)
}
pub async fn list_watched_repositories(
    pool: &SqlitePool,
    user_id: i64,
    viewer: Option<i64>,
    limit: i64,
    offset: i64,
) -> Result<Vec<Repository>> {
    let sql=format!("SELECT r.* FROM watches w JOIN repositories r ON r.id=w.repo_id LEFT JOIN users u ON u.id=?2 WHERE w.user_id=?1 AND w.level IN ('watching','custom') AND {} ORDER BY w.created_at DESC,w.repo_id DESC LIMIT ?3 OFFSET ?4",readable_condition());
    Ok(sqlx::query_as(&sql)
        .bind(user_id)
        .bind(viewer)
        .bind(limit)
        .bind(offset)
        .fetch_all(pool)
        .await?)
}

pub async fn watch_repo(
    pool: &SqlitePool,
    user_id: impl Into<codeyard_db_core::queries::credentials::Actor>,
    repo_id: i64,
) -> Result<bool> {
    let (mut tx, user_id) = begin_change(pool, user_id.into(), repo_id).await?;
    let result = sqlx::query("INSERT INTO watches (user_id, repo_id, level) VALUES (?1,?2,'watching') ON CONFLICT (user_id,repo_id) DO UPDATE SET level='watching',events='' WHERE watches.level<>'watching'")
        .bind(user_id).bind(repo_id).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(result.rows_affected() > 0)
}

pub async fn unwatch_repo(
    pool: &SqlitePool,
    user_id: impl Into<codeyard_db_core::queries::credentials::Actor>,
    repo_id: i64,
) -> Result<bool> {
    let (mut tx, user_id) = begin_removal(pool, user_id.into()).await?;
    let result = sqlx::query("DELETE FROM watches WHERE user_id = ?1 AND repo_id = ?2")
        .bind(user_id)
        .bind(repo_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;

    Ok(result.rows_affected() > 0)
}

pub async fn is_repo_watched(pool: &SqlitePool, user_id: i64, repo_id: i64) -> Result<bool> {
    let watch = sqlx::query_as::<_, Watch>(
        "SELECT * FROM watches WHERE user_id = ?1 AND repo_id = ?2 AND level IN ('watching','custom')",
    )
    .bind(user_id)
    .bind(repo_id)
    .fetch_optional(pool)
    .await?;

    Ok(watch.is_some())
}

pub async fn star_repo(
    pool: &SqlitePool,
    user_id: impl Into<codeyard_db_core::queries::credentials::Actor>,
    repo_id: i64,
) -> Result<bool> {
    let (mut tx, user_id) = begin_change(pool, user_id.into(), repo_id).await?;
    let result =
        sqlx::query("INSERT INTO stars (user_id, repo_id) VALUES (?1, ?2) ON CONFLICT DO NOTHING")
            .bind(user_id)
            .bind(repo_id)
            .execute(&mut *tx)
            .await?;
    tx.commit().await?;
    Ok(result.rows_affected() > 0)
}

pub async fn unstar_repo(
    pool: &SqlitePool,
    user_id: impl Into<codeyard_db_core::queries::credentials::Actor>,
    repo_id: i64,
) -> Result<bool> {
    let (mut tx, user_id) = begin_removal(pool, user_id.into()).await?;
    let result = sqlx::query("DELETE FROM stars WHERE user_id = ?1 AND repo_id = ?2")
        .bind(user_id)
        .bind(repo_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;

    Ok(result.rows_affected() > 0)
}

pub async fn starred_at(
    pool: &SqlitePool,
    user_id: i64,
    repo_id: i64,
) -> Result<Option<chrono::DateTime<chrono::Utc>>> {
    Ok(
        sqlx::query_scalar("SELECT created_at FROM stars WHERE user_id = ?1 AND repo_id = ?2")
            .bind(user_id)
            .bind(repo_id)
            .fetch_optional(pool)
            .await?,
    )
}

pub async fn is_repo_starred(pool: &SqlitePool, user_id: i64, repo_id: i64) -> Result<bool> {
    let star = sqlx::query_as::<_, Star>("SELECT * FROM stars WHERE user_id = ?1 AND repo_id = ?2")
        .bind(user_id)
        .bind(repo_id)
        .fetch_optional(pool)
        .await?;

    Ok(star.is_some())
}

pub async fn list_repo_stargazers(
    pool: &SqlitePool,
    repo_id: i64,
    limit: i64,
    offset: i64,
) -> Result<Vec<User>> {
    anyhow::ensure!(offset >= 0, "Page offset must be nonnegative");
    let users = sqlx::query_as::<_, User>(
        r#"
        SELECT u.* FROM users u
        INNER JOIN stars s ON u.id = s.user_id
        WHERE s.repo_id = ?1
        ORDER BY s.created_at DESC, s.user_id DESC
        LIMIT ?2 OFFSET ?3
        "#,
    )
    .bind(repo_id)
    .bind(limit.clamp(1, 100))
    .bind(offset)
    .fetch_all(pool)
    .await?;

    Ok(users)
}

/// One page of the repositories `starred_by` starred whose name or description contains `text`
/// and that `viewer` can read, in `sort` order. The access check is part of the query, so a
/// repository the viewer lost access to is never counted or listed.
pub async fn list_starred_repositories(
    pool: &SqlitePool,
    starred_by: i64,
    viewer: Option<i64>,
    text: &str,
    sort: StarSort,
    limit: i64,
    offset: i64,
) -> Result<Vec<Repository>> {
    anyhow::ensure!(limit >= 0, "Result limit must be nonnegative");
    anyhow::ensure!(offset >= 0, "Result offset must be nonnegative");
    let order = match sort {
        StarSort::Recent => "s.created_at DESC, s.repo_id DESC",
        StarSort::Oldest => "s.created_at, s.repo_id",
        StarSort::Updated => "r.updated_at DESC, r.id DESC",
        StarSort::UpdatedOldest => "r.updated_at, r.id",
        StarSort::Stars => "r.stars_count DESC, s.created_at DESC, s.repo_id DESC",
        StarSort::Name => "LOWER(r.name), r.id",
    };
    let sql = format!(
        "SELECT r.* FROM stars s JOIN repositories r ON r.id = s.repo_id \
         LEFT JOIN users u ON u.id = ?2 \
         WHERE s.user_id = ?1 AND {readable} \
         AND (r.name REGEXP ?3 OR r.description REGEXP ?3) \
         ORDER BY {order} LIMIT ?4 OFFSET ?5",
        readable = readable_condition(),
    );
    Ok(sqlx::query_as::<_, Repository>(&sql)
        .bind(starred_by)
        .bind(viewer)
        .bind(contains(text))
        .bind(limit)
        .bind(offset)
        .fetch_all(pool)
        .await?)
}

/// How many repositories `list_starred_repositories` lists for the same arguments.
pub async fn count_starred_repositories(
    pool: &SqlitePool,
    starred_by: i64,
    viewer: Option<i64>,
    text: &str,
) -> Result<i64> {
    let sql = format!(
        "SELECT COUNT(*) FROM stars s JOIN repositories r ON r.id = s.repo_id \
         LEFT JOIN users u ON u.id = ?2 \
         WHERE s.user_id = ?1 AND {readable} \
         AND (r.name REGEXP ?3 OR r.description REGEXP ?3)",
        readable = readable_condition(),
    );
    Ok(sqlx::query_scalar::<_, i64>(&sql)
        .bind(starred_by)
        .bind(viewer)
        .bind(contains(text))
        .fetch_one(pool)
        .await?)
}

/// Saves a watch level. `custom` keeps `events`; the other levels clear them.
pub async fn set_watch_level(
    pool: &SqlitePool,
    user_id: impl Into<codeyard_db_core::queries::credentials::Actor>,
    repo_id: i64,
    level: &str,
    events: &str,
) -> Result<()> {
    let events = if level == "custom" { events } else { "" };
    let (mut tx, user_id) = begin_change(pool, user_id.into(), repo_id).await?;
    sqlx::query(
        r#"
        INSERT INTO watches (user_id, repo_id, level, events)
        VALUES (?1, ?2, ?3, ?4)
        ON CONFLICT (user_id, repo_id)
        DO UPDATE SET level = excluded.level, events = excluded.events
        "#,
    )
    .bind(user_id)
    .bind(repo_id)
    .bind(level)
    .bind(events)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

/// The viewer's level and custom events for a repository, or `None` when they only participate.
pub async fn get_watch_level(
    pool: &SqlitePool,
    user_id: i64,
    repo_id: i64,
) -> Result<Option<(String, String)>> {
    Ok(
        sqlx::query_as("SELECT level, events FROM watches WHERE user_id = ?1 AND repo_id = ?2")
            .bind(user_id)
            .bind(repo_id)
            .fetch_optional(pool)
            .await?,
    )
}

/// Up to `limit` users after `after_user_id`, in user order, who want a new `event` ("issues",
/// "pulls" or "releases"): "All activity" watchers and custom watchers who picked it. Ignoring users are left
/// out, and so are banned or restricted accounts and accounts that cannot read the repository.
pub async fn list_event_watchers(
    pool: &SqlitePool,
    repo_id: i64,
    event: &str,
    after_user_id: i64,
    limit: i64,
) -> Result<Vec<i64>> {
    let sql = format!(
        "SELECT w.user_id FROM watches w
         JOIN users u ON u.id = w.user_id AND NOT u.is_banned AND NOT u.must_change_password
         JOIN repositories r ON r.id = w.repo_id
         WHERE w.repo_id = ?1 AND w.user_id > ?2 AND w.level <> 'ignoring'
           AND (w.level = 'watching' OR (',' || w.events || ',') LIKE '%,' || ?3 || ',%')
           AND {readable}
         ORDER BY w.user_id LIMIT ?4",
        readable = readable_condition(),
    );
    Ok(sqlx::query_scalar::<_, i64>(&sql)
        .bind(repo_id)
        .bind(after_user_id)
        .bind(event)
        .bind(limit)
        .fetch_all(pool)
        .await?)
}
