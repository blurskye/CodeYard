use crate::backend::db::models::Notification;
use crate::backend::db::notifications::records::{
    NotificationCounts, NotificationCursor, NotificationFilter, NotificationMark,
    NotificationScopes,
};
use crate::backend::db::notifications::{
    NewNotification, NotificationInbox, NotificationInboxAction,
};
use crate::backend::db::sqlite::repositories::access::USER_REPO_ACCESS_SQL;
use crate::backend::db::sqlite::search::contains;
use anyhow::Result;
use sqlx::SqlitePool;

pub async fn create_notification(
    pool: &SqlitePool,
    input: NewNotification<'_>,
) -> Result<Notification> {
    use crate::backend::db::notifications::records::NotificationSuppressed;
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    if matches!(input.kind, "user_followed" | "repository_starred") {
        let recent: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM notifications WHERE user_id=?1 AND actor_id=?2 AND kind=?3 AND (?3='user_followed' OR repo_id=?4) AND created_at>strftime('%Y-%m-%dT%H:%M:%f000Z','now','-1 day'))")
            .bind(input.user_id).bind(input.actor_id).bind(input.kind).bind(input.repo_id).fetch_one(&mut *tx).await?;
        anyhow::ensure!(!recent, NotificationSuppressed);
    }
    if matches!(input.kind, "issue_comment" | "mention") {
        if let Some(comment_id) = input
            .metadata
            .get("comment_id")
            .and_then(serde_json::Value::as_i64)
        {
            let comment: Option<i64> = sqlx::query_scalar(
                "SELECT c.id FROM issue_comments c JOIN issues i ON i.id=c.issue_id
                 WHERE c.id=?1 AND i.repo_id=?2",
            )
            .bind(comment_id)
            .bind(input.repo_id)
            .fetch_optional(&mut *tx)
            .await?;
            anyhow::ensure!(comment.is_some(), NotificationSuppressed);
        }
    }
    let pull_comment_id = match input.kind {
        "pull_request_comment" | "pull_request_comment_reply" => input.metadata.get("comment_id"),
        "mention" => input.metadata.get("pull_request_comment_id"),
        _ => None,
    }
    .and_then(serde_json::Value::as_i64);
    if let Some(comment_id) = pull_comment_id {
        let comment: Option<i64> = sqlx::query_scalar(
            "SELECT c.id FROM pr_comments c JOIN pull_requests p ON p.id=c.pull_request_id
             WHERE c.id=?1 AND p.repo_id=?2 AND NOT EXISTS(SELECT 1 FROM pr_reviews v WHERE v.id=c.review_id AND v.state='pending')",
        )
        .bind(comment_id)
        .bind(input.repo_id)
        .fetch_optional(&mut *tx)
        .await?;
        anyhow::ensure!(comment.is_some(), NotificationSuppressed);
    }
    let notification = sqlx::query_as::<_, Notification>(
        r#"
        INSERT INTO notifications (user_id, actor_id, repo_id, kind, title, body, link, metadata, scope_repository_id)
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, COALESCE(?3,0))
        RETURNING *
        "#,
    )
    .bind(input.user_id)
    .bind(input.actor_id)
    .bind(input.repo_id)
    .bind(input.kind)
    .bind(input.title)
    .bind(input.body)
    .bind(input.link)
    .bind(serde_json::to_string(input.metadata)?)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(notification)
}

/// Whether the recipient is an active account. Callers read it inside their transaction.
async fn notification_recipient(
    connection: &mut sqlx::SqliteConnection,
    user_id: i64,
) -> Result<bool> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM users WHERE id=?1 AND NOT is_banned AND NOT must_change_password)",
    )
    .bind(user_id)
    .fetch_one(connection)
    .await?)
}

// Keep SQLite's exact persisted ordering value, not a re-encoded timestamp.
type NotificationCandidate = (i64, i64, String);

/// Whether the recipient can read the repository a notification names, as the list has always
/// decided it. The owner must exist and be well formed, even for a public repository. Then the
/// recipient needs Read, or is a site admin over a personal repository. Canonical Read includes
/// archived and suspended-governance reads, as `USER_REPO_ACCESS_SQL` computes them.
fn scope_readable_condition() -> String {
    format!(
        "((r.owner_kind='user' AND r.owner_user_id IS NOT NULL AND r.owner_org_id IS NULL
            AND EXISTS (SELECT 1 FROM users ow WHERE ow.id=r.owner_user_id))
          OR (r.owner_kind='org' AND r.owner_user_id IS NULL AND r.owner_org_id IS NOT NULL
            AND EXISTS (SELECT 1 FROM organizations og WHERE og.id=r.owner_org_id)))
         AND (({USER_REPO_ACCESS_SQL}) >= 1 OR (r.owner_kind='user' AND u.is_admin))"
    )
}

/// Decides every repository the recipient's notifications point at, in one statement. A
/// repository that is gone is unreadable, because the scope outlives the repository.
async fn notification_scopes(
    connection: &mut sqlx::SqliteConnection,
    user_id: i64,
) -> Result<NotificationScopes> {
    decide_scopes(connection, user_id, false, None).await
}

async fn decide_scopes(
    connection: &mut sqlx::SqliteConnection,
    user_id: i64,
    unread_only: bool,
    notification_id: Option<i64>,
) -> Result<NotificationScopes> {
    let readable = scope_readable_condition();
    let decided: Vec<(i64, bool)> = sqlx::query_as(&format!(
        "SELECT s.scope, EXISTS(
            SELECT 1 FROM repositories r
            JOIN users u ON u.id=?1 AND NOT u.is_banned AND NOT u.must_change_password
            WHERE r.id=s.scope AND {readable})
         FROM (SELECT DISTINCT scope_repository_id AS scope FROM notifications
               WHERE user_id=?1 AND scope_repository_id>0
                 AND (NOT ?2 OR (read_at IS NULL AND done_at IS NULL) OR id=?3)) s"
    ))
    .bind(user_id)
    .bind(unread_only)
    .bind(notification_id)
    .fetch_all(&mut *connection)
    .await?;
    Ok(NotificationScopes {
        high_water: notification_high_water(connection, user_id).await?,
        readable: decided.into_iter().collect(),
    })
}

/// Whether the recipient can read the scope, as `notification_scopes` decided it. The account
/// scope, 0, is always readable.
fn scope_readable(scopes: &NotificationScopes, scope_repository_id: i64) -> bool {
    scope_repository_id == 0
        || scopes
            .readable
            .get(&scope_repository_id)
            .copied()
            .unwrap_or(false)
}

async fn notification_high_water(
    connection: &mut sqlx::SqliteConnection,
    user_id: i64,
) -> Result<i64> {
    Ok(
        sqlx::query_scalar("SELECT COALESCE(MAX(id),0) FROM notifications WHERE user_id=?1")
            .bind(user_id)
            .fetch_one(connection)
            .await?,
    )
}

/// The filter as SQL. It binds ?3 unread only, ?4 the view, ?5 the repository, ?6 the kinds
/// as a JSON array and ?7 the search pattern; each query names its other parameters itself.
/// The pattern is case-insensitive for Unicode, which SQLite's lower() is not.
const FILTER_SQL: &str = "(NOT ?3 OR (read_at IS NULL AND done_at IS NULL))
  AND ((?4='inbox' AND done_at IS NULL)
    OR (?4='saved' AND saved_at IS NOT NULL AND done_at IS NULL)
    OR (?4='done' AND done_at IS NOT NULL))
  AND (?5 IS NULL OR scope_repository_id=?5)
  AND (json_array_length(?6)=0 OR kind IN (SELECT value FROM json_each(?6)))
  AND (?7 IS NULL OR title REGEXP ?7 OR json_extract(metadata,'$.subject') REGEXP ?7
    OR EXISTS (SELECT 1 FROM repositories r
               WHERE r.id=notifications.scope_repository_id AND r.name REGEXP ?7))";

async fn notification_candidates(
    connection: &mut sqlx::SqliteConnection,
    user_id: i64,
    high_water: i64,
    filter: &NotificationFilter,
    search: Option<&str>,
    unreadable: Vec<i64>,
    cursor: Option<&NotificationCandidate>,
) -> Result<Vec<NotificationCandidate>> {
    let sql = format!(
        "SELECT id,scope_repository_id,created_at FROM notifications
         WHERE user_id=?1 AND id <= ?2 AND {FILTER_SQL}
           AND scope_repository_id NOT IN (SELECT value FROM json_each(?8))
           AND (?9 IS NULL OR (created_at,id) < (?9,?10))
         ORDER BY created_at DESC,id DESC LIMIT 100"
    );
    Ok(sqlx::query_as(&sql)
        .bind(user_id)
        .bind(high_water)
        .bind(filter.unread_only)
        .bind(filter.view.as_str())
        .bind(filter.repository_id)
        .bind(serde_json::to_string(&filter.kinds)?)
        .bind(search)
        .bind(serde_json::to_string(&unreadable)?)
        .bind(cursor.map(|c| c.2.as_str()))
        .bind(cursor.map(|c| c.0))
        .fetch_all(connection)
        .await?)
}

pub async fn list_notifications_for_user(
    pool: &SqlitePool,
    user_id: i64,
    limit: i64,
    unread_only: bool,
) -> Result<Vec<Notification>> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    if !notification_recipient(&mut tx, user_id).await? {
        tx.commit().await?;
        return Ok(Vec::new());
    }
    let scopes = notification_scopes(&mut tx, user_id).await?;
    let (mut notifications, _) = notification_list(
        &mut tx,
        user_id,
        &scopes,
        &NotificationFilter::inbox(unread_only),
        limit,
        None,
    )
    .await?;
    refresh_links(&mut tx, &mut notifications).await?;
    tx.commit().await?;
    Ok(notifications)
}

async fn notification_list(
    connection: &mut sqlx::SqliteConnection,
    user_id: i64,
    scopes: &NotificationScopes,
    filter: &NotificationFilter,
    limit: i64,
    before: Option<NotificationCursor>,
) -> Result<(Vec<Notification>, Option<NotificationCursor>)> {
    let limit = limit.clamp(1, 100) as usize;
    let search = filter.search_text().map(|text| contains(&text));
    // The page position is (created_at, id); the scope slot is never read.
    let mut cursor = before.map(|c| (c.id, 0, c.created_at));
    let mut last_visible = None;
    let mut notifications = Vec::with_capacity(limit);
    loop {
        let page = notification_candidates(
            connection,
            user_id,
            scopes.high_water,
            filter,
            search.as_deref(),
            scopes.unreadable(),
            cursor.as_ref(),
        )
        .await?;
        if page.is_empty() {
            break;
        }
        let mut authorized = Vec::new();
        let mut more = false;
        for candidate in page {
            if scope_readable(scopes, candidate.1) {
                if notifications.len() + authorized.len() == limit {
                    more = true;
                    break;
                }
                authorized.push(candidate.clone());
            }
            cursor = Some(candidate);
        }
        let ids: Vec<i64> = authorized.iter().map(|c| c.0).collect();
        if !ids.is_empty() {
            let rows: Vec<Notification> = sqlx::query_as(
                "SELECT * FROM notifications WHERE user_id=?1 AND id IN (SELECT value FROM json_each(?2))",
            ).bind(user_id).bind(serde_json::to_string(&ids)?).fetch_all(&mut *connection).await?;
            let mut rows: std::collections::BTreeMap<_, _> =
                rows.into_iter().map(|n| (n.id, n)).collect();
            for candidate in authorized {
                if let Some(notification) = rows.remove(&candidate.0) {
                    last_visible = Some(NotificationCursor {
                        created_at: candidate.2,
                        id: candidate.0,
                    });
                    notifications.push(notification);
                }
            }
        }
        if more && notifications.len() == limit {
            return Ok((notifications, last_visible));
        }
    }
    Ok((notifications, None))
}

/// Unread rows grouped by repository, ordered by repository ID. Each group needs one
/// authorization, however many notifications it holds.
async fn unread_groups(
    connection: &mut sqlx::SqliteConnection,
    user_id: i64,
    high_water: i64,
) -> Result<Vec<(i64, i64)>> {
    Ok(sqlx::query_as(
        "SELECT scope_repository_id,COUNT(*) FROM notifications
         WHERE user_id=?1 AND id<=?2 AND read_at IS NULL
         GROUP BY scope_repository_id ORDER BY scope_repository_id",
    )
    .bind(user_id)
    .bind(high_water)
    .fetch_all(connection)
    .await?)
}

/// Unread inbox rows grouped by repository, ordered by repository ID. The unread badge
/// authorizes only the repositories that still have one.
async fn unread_inbox_groups(
    connection: &mut sqlx::SqliteConnection,
    user_id: i64,
    high_water: i64,
) -> Result<Vec<(i64, i64)>> {
    Ok(sqlx::query_as(
        "SELECT scope_repository_id,COUNT(*) FROM notifications
         WHERE user_id=?1 AND id<=?2 AND read_at IS NULL AND done_at IS NULL
         GROUP BY scope_repository_id ORDER BY scope_repository_id",
    )
    .bind(user_id)
    .bind(high_water)
    .fetch_all(connection)
    .await?)
}

/// Unread notifications in the inbox. Done rows are not counted.
async fn notification_unread_count(
    connection: &mut sqlx::SqliteConnection,
    user_id: i64,
    scopes: &NotificationScopes,
) -> Result<i64> {
    let mut count = 0;
    for (scope, unread) in unread_inbox_groups(connection, user_id, scopes.high_water).await? {
        if scope_readable(scopes, scope) {
            count += unread;
        }
    }
    Ok(count)
}

/// Counts per repository scope, ordered by repository ID: inbox, saved and done rows. The
/// grouping reads no authority; `notification_counts` decides each group.
async fn notification_scope_counts(
    connection: &mut sqlx::SqliteConnection,
    user_id: i64,
    high_water: i64,
) -> Result<Vec<(i64, i64, i64, i64)>> {
    Ok(sqlx::query_as(
        "SELECT scope_repository_id,
                COUNT(*) FILTER (WHERE done_at IS NULL),
                COUNT(*) FILTER (WHERE done_at IS NULL AND saved_at IS NOT NULL),
                COUNT(*) FILTER (WHERE done_at IS NOT NULL)
         FROM notifications WHERE user_id=?1 AND id<=?2
         GROUP BY scope_repository_id ORDER BY scope_repository_id",
    )
    .bind(user_id)
    .bind(high_water)
    .fetch_all(connection)
    .await?)
}

/// The tab counts and the repositories with notifications, over what the recipient can read.
async fn notification_counts(
    connection: &mut sqlx::SqliteConnection,
    user_id: i64,
    scopes: &NotificationScopes,
) -> Result<NotificationCounts> {
    let names = readable_repository_names(connection, scopes).await?;
    let mut counts = NotificationCounts::default();
    for (scope, inbox, saved, done) in
        notification_scope_counts(connection, user_id, scopes.high_water).await?
    {
        if scope_readable(scopes, scope) {
            counts.views.inbox += inbox;
            counts.views.saved += saved;
            counts.views.done += done;
            if let Some(name) = names.get(&scope) {
                counts.repositories.push((scope, name.clone()));
            }
        }
    }
    Ok(counts)
}

/// `owner/name` for each repository the recipient can read among the scopes. It is read in
/// the transaction that decided the scopes, so a repository revoked since is never named.
async fn readable_repository_names(
    connection: &mut sqlx::SqliteConnection,
    scopes: &NotificationScopes,
) -> Result<std::collections::BTreeMap<i64, String>> {
    let readable: Vec<i64> = scopes
        .readable
        .iter()
        .filter(|(_, readable)| **readable)
        .map(|(scope, _)| *scope)
        .collect();
    let rows: Vec<(i64, String)> = sqlx::query_as(
        "SELECT r.id, COALESCE(ou.username, oo.name) || '/' || r.name
         FROM repositories r
         LEFT JOIN users ou ON r.owner_kind='user' AND ou.id=r.owner_user_id
         LEFT JOIN organizations oo ON r.owner_kind='org' AND oo.id=r.owner_org_id
         WHERE r.id IN (SELECT value FROM json_each(?1))",
    )
    .bind(serde_json::to_string(&readable)?)
    .fetch_all(connection)
    .await?;
    Ok(rows.into_iter().collect())
}

pub async fn count_unread_notifications(pool: &SqlitePool, user_id: i64) -> Result<i64> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let count = if notification_recipient(&mut tx, user_id).await? {
        let scopes = notification_scopes(&mut tx, user_id).await?;
        notification_unread_count(&mut tx, user_id, &scopes).await?
    } else {
        0
    };
    tx.commit().await?;
    Ok(count)
}

/// The header count authorizes only scopes that still have unread inbox notifications.
pub async fn count_header_unread(pool: &SqlitePool, user_id: i64) -> Result<i64> {
    let readable = scope_readable_condition();
    Ok(sqlx::query_scalar(&format!(
        "SELECT COUNT(*) FROM notifications
         WHERE user_id=?1 AND read_at IS NULL AND done_at IS NULL
           AND EXISTS(SELECT 1 FROM users WHERE id=?1 AND NOT is_banned AND NOT must_change_password)
           AND (scope_repository_id=0 OR scope_repository_id IN (
               SELECT r.id FROM repositories r JOIN users u ON u.id=?1
               WHERE r.id IN (SELECT scope_repository_id FROM notifications
                              WHERE user_id=?1 AND read_at IS NULL AND done_at IS NULL
                                AND scope_repository_id>0)
                 AND {readable}))"
    )).bind(user_id).fetch_one(pool).await?)
}

async fn refresh_links(
    connection: &mut sqlx::SqliteConnection,
    notifications: &mut [Notification],
) -> Result<()> {
    let ids: Vec<_> = notifications.iter().filter_map(|n| n.repo_id).collect();
    let names: Vec<(i64,String,String)> = sqlx::query_as("SELECT r.id,COALESCE(u.username,o.name),r.name FROM repositories r LEFT JOIN users u ON r.owner_kind='user' AND u.id=r.owner_user_id LEFT JOIN organizations o ON r.owner_kind='org' AND o.id=r.owner_org_id WHERE r.id IN (SELECT value FROM json_each(?1))")
        .bind(serde_json::to_string(&ids)?).fetch_all(connection).await?;
    for notification in notifications {
        if let Some((_, owner, name)) = names
            .iter()
            .find(|(id, _, _)| Some(*id) == notification.repo_id)
        {
            crate::backend::db::notifications::records::refresh_link(notification, owner, name);
        }
    }
    Ok(())
}

pub async fn notification_delivery(
    pool: &SqlitePool,
    session_id: &str,
    user_id: i64,
    notification_id: Option<i64>,
) -> Result<Option<crate::backend::db::notifications::NotificationDelivery>> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    if !notification_recipient(&mut tx, user_id).await? {
        return Ok(None);
    }
    let expiry: Option<String> = sqlx::query_scalar(
        "SELECT expires_at FROM sessions WHERE id=?1 AND user_id=?2 AND expires_at>strftime('%Y-%m-%dT%H:%M:%f000Z','now')",
    ).bind(session_id).bind(user_id).fetch_optional(&mut *tx).await?;
    let Some(expiry) = expiry else {
        return Ok(None);
    };
    let scopes = decide_scopes(&mut tx, user_id, true, notification_id).await?;
    let mut notification = None;
    if let Some(id) = notification_id {
        let scope: Option<i64> = sqlx::query_scalar(
            "SELECT scope_repository_id FROM notifications WHERE id=?1 AND user_id=?2",
        )
        .bind(id)
        .bind(user_id)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(scope) = scope {
            if scope_readable(&scopes, scope) {
                notification = sqlx::query_as("SELECT * FROM notifications WHERE id=?1 AND user_id=?2 AND scope_repository_id = ?3")
                    .bind(id).bind(user_id).bind(scope).fetch_optional(&mut *tx).await?;
            }
        }
    }
    let unread_count = notification_unread_count(&mut tx, user_id, &scopes).await?;
    if let Some(notification) = notification.as_mut() {
        refresh_links(&mut tx, std::slice::from_mut(notification)).await?;
    }
    let alive: bool = sqlx::query_scalar("SELECT ?1>strftime('%Y-%m-%dT%H:%M:%f000Z','now')")
        .bind(expiry)
        .fetch_one(&mut *tx)
        .await?;
    if !alive {
        return Ok(None);
    }
    tx.commit().await?;
    Ok(Some(
        crate::backend::db::notifications::NotificationDelivery {
            notification,
            unread_count,
        },
    ))
}

pub async fn mark_notification_read(
    pool: &SqlitePool,
    user_id: i64,
    notification_id: i64,
) -> Result<Option<Notification>> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    if !notification_recipient(&mut tx, user_id).await? {
        tx.commit().await?;
        return Ok(None);
    }
    let scopes = notification_scopes(&mut tx, user_id).await?;
    let (notification, _) = notification_read(&mut tx, user_id, &scopes, notification_id).await?;
    tx.commit().await?;
    Ok(notification)
}

async fn notification_read(
    connection: &mut sqlx::SqliteConnection,
    user_id: i64,
    scopes: &NotificationScopes,
    notification_id: i64,
) -> Result<(Option<Notification>, u64)> {
    let scope: Option<(i64, bool)> = sqlx::query_as(
        "SELECT scope_repository_id,read_at IS NULL FROM notifications WHERE id=?1 AND user_id=?2",
    )
    .bind(notification_id)
    .bind(user_id)
    .fetch_optional(&mut *connection)
    .await?;
    if let Some((scope_repository_id, unread)) = scope {
        if scope_readable(scopes, scope_repository_id) {
            let notification = sqlx::query_as::<_, Notification>(
                "UPDATE notifications SET read_at=COALESCE(read_at,strftime('%Y-%m-%dT%H:%M:%f000Z','now')) WHERE id=?1 AND user_id=?2 AND scope_repository_id = ?3 RETURNING *",
            ).bind(notification_id).bind(user_id).bind(scope_repository_id).fetch_optional(connection).await?;
            let updated = u64::from(unread && notification.is_some());
            return Ok((notification, updated));
        }
    }
    Ok((None, 0))
}

/// Sets or clears a mark on a notification the recipient can read. Setting a mark that
/// is already set keeps its first time, so the change count is zero.
async fn notification_mark(
    connection: &mut sqlx::SqliteConnection,
    user_id: i64,
    scopes: &NotificationScopes,
    notification_id: i64,
    mark: NotificationMark,
    on: bool,
) -> Result<(Option<Notification>, u64)> {
    let column = mark.column();
    let scope: Option<(i64, bool)> = sqlx::query_as(&format!(
        "SELECT scope_repository_id,{column} IS NOT NULL FROM notifications WHERE id=?1 AND user_id=?2"
    ))
    .bind(notification_id)
    .bind(user_id)
    .fetch_optional(&mut *connection)
    .await?;
    if let Some((scope_repository_id, was_set)) = scope {
        if scope_readable(scopes, scope_repository_id) {
            let notification = sqlx::query_as::<_, Notification>(&format!(
                "UPDATE notifications SET {column}=CASE WHEN ?3 THEN COALESCE({column},strftime('%Y-%m-%dT%H:%M:%f000Z','now')) ELSE NULL END
                 WHERE id=?1 AND user_id=?2 AND scope_repository_id=?4 RETURNING *"
            ))
            .bind(notification_id)
            .bind(user_id)
            .bind(on)
            .bind(scope_repository_id)
            .fetch_optional(connection)
            .await?;
            let updated = u64::from(was_set != on && notification.is_some());
            return Ok((notification, updated));
        }
    }
    Ok((None, 0))
}

pub async fn mark_all_notifications_read(pool: &SqlitePool, user_id: i64) -> Result<u64> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    if !notification_recipient(&mut tx, user_id).await? {
        tx.commit().await?;
        return Ok(0);
    }
    let scopes = notification_scopes(&mut tx, user_id).await?;
    let count = notification_read_all(&mut tx, user_id, &scopes).await?;
    tx.commit().await?;
    Ok(count)
}

async fn notification_read_all(
    connection: &mut sqlx::SqliteConnection,
    user_id: i64,
    scopes: &NotificationScopes,
) -> Result<u64> {
    let mut count = 0;
    for (scope, _) in unread_groups(connection, user_id, scopes.high_water).await? {
        if scope_readable(scopes, scope) {
            count += sqlx::query(
                "UPDATE notifications SET read_at=strftime('%Y-%m-%dT%H:%M:%f000Z','now') WHERE id<=?1 AND user_id=?2 AND read_at IS NULL AND scope_repository_id = ?3",
            ).bind(scopes.high_water).bind(user_id).bind(scope).execute(&mut *connection).await?.rows_affected();
        }
    }
    Ok(count)
}

/// Marks every notification of the filter as done, in each repository the recipient can
/// read. Hidden repositories are skipped, so their history is never changed.
async fn notification_done_all(
    connection: &mut sqlx::SqliteConnection,
    user_id: i64,
    scopes: &NotificationScopes,
    filter: &NotificationFilter,
) -> Result<u64> {
    let sql = format!(
        "UPDATE notifications SET done_at=strftime('%Y-%m-%dT%H:%M:%f000Z','now')
         WHERE user_id=?1 AND id<=?2 AND done_at IS NULL AND scope_repository_id=?8 AND {FILTER_SQL}"
    );
    let kinds = serde_json::to_string(&filter.kinds)?;
    let search = filter.search_text().map(|text| contains(&text));
    let mut count = 0;
    for (scope, _, _, _) in
        notification_scope_counts(connection, user_id, scopes.high_water).await?
    {
        if scope_readable(scopes, scope) {
            count += sqlx::query(&sql)
                .bind(user_id)
                .bind(scopes.high_water)
                .bind(filter.unread_only)
                .bind(filter.view.as_str())
                .bind(filter.repository_id)
                .bind(&kinds)
                .bind(search.as_deref())
                .bind(scope)
                .execute(&mut *connection)
                .await?
                .rows_affected();
        }
    }
    Ok(count)
}

pub async fn notification_inbox(
    pool: &SqlitePool,
    session_id: &str,
    user_id: i64,
    action: NotificationInboxAction,
) -> Result<Option<NotificationInbox>> {
    // Reserved writer ownership precedes every authority read, including waiters.
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    if !notification_recipient(&mut tx, user_id).await? {
        tx.rollback().await?;
        return Ok(None);
    }
    let expiry: Option<String> = sqlx::query_scalar(
        "SELECT expires_at FROM sessions WHERE id=?1 AND user_id=?2 AND expires_at>strftime('%Y-%m-%dT%H:%M:%f000Z','now')",
    ).bind(session_id).bind(user_id).fetch_optional(&mut *tx).await?;
    let Some(expiry) = expiry else {
        tx.rollback().await?;
        return Ok(None);
    };
    let scopes = notification_scopes(&mut tx, user_id).await?;
    let mut next_cursor = None;
    let wants_counts = matches!(action, NotificationInboxAction::List { .. });
    let (mut notifications, updated) = match action {
        NotificationInboxAction::List {
            filter,
            limit,
            before,
        } => {
            let (notifications, next) =
                notification_list(&mut tx, user_id, &scopes, &filter, limit, before).await?;
            next_cursor = next;
            (notifications, 0)
        }
        NotificationInboxAction::Read { notification_id } => {
            let (notification, updated) =
                notification_read(&mut tx, user_id, &scopes, notification_id).await?;
            (notification.into_iter().collect(), updated)
        }
        NotificationInboxAction::ReadAll => (
            Vec::new(),
            notification_read_all(&mut tx, user_id, &scopes).await?,
        ),
        NotificationInboxAction::Save {
            notification_id,
            saved,
        } => {
            let (notification, updated) = notification_mark(
                &mut tx,
                user_id,
                &scopes,
                notification_id,
                NotificationMark::Saved,
                saved,
            )
            .await?;
            (notification.into_iter().collect(), updated)
        }
        NotificationInboxAction::Done {
            notification_id,
            done,
        } => {
            let (notification, updated) = notification_mark(
                &mut tx,
                user_id,
                &scopes,
                notification_id,
                NotificationMark::Done,
                done,
            )
            .await?;
            (notification.into_iter().collect(), updated)
        }
        NotificationInboxAction::DoneAll { filter } => (
            Vec::new(),
            notification_done_all(&mut tx, user_id, &scopes, &filter).await?,
        ),
    };
    let unread_count = notification_unread_count(&mut tx, user_id, &scopes).await?;
    let counts = if wants_counts {
        Some(notification_counts(&mut tx, user_id, &scopes).await?)
    } else {
        None
    };
    refresh_links(&mut tx, &mut notifications).await?;
    let alive: bool = sqlx::query_scalar("SELECT ?1>strftime('%Y-%m-%dT%H:%M:%f000Z','now')")
        .bind(expiry)
        .fetch_one(&mut *tx)
        .await?;
    if !alive {
        // Acknowledgments are provisional until final original-session expiry.
        tx.rollback().await?;
        return Ok(None);
    }
    tx.commit().await?;
    Ok(Some(NotificationInbox {
        notifications,
        next_cursor,
        unread_count,
        counts,
        updated,
    }))
}
