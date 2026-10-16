use crate::backend::db::models::Notification;
use crate::backend::db::notifications::records::{NotificationCursor, NotificationScopes};
use crate::backend::db::notifications::{
    NewNotification, NotificationInbox, NotificationInboxAction,
};
use anyhow::Result;
use sqlx::SqlitePool;

pub async fn create_notification(
    pool: &SqlitePool,
    input: NewNotification<'_>,
) -> Result<Notification> {
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
    .fetch_one(pool)
    .await?;

    Ok(notification)
}

// Every caller starts BEGIN IMMEDIATE before reading any authority or payload.
async fn notification_recipient(
    connection: &mut sqlx::SqliteConnection,
    user_id: i64,
) -> Result<Option<bool>> {
    Ok(sqlx::query_scalar(
        "SELECT is_admin FROM users WHERE id=?1 AND NOT is_banned AND NOT must_change_password",
    )
    .bind(user_id)
    .fetch_optional(connection)
    .await?)
}

async fn notification_scope_readable(
    connection: &mut sqlx::SqliteConnection,
    user_id: i64,
    admin: bool,
    scope_repository_id: Option<i64>,
) -> Result<bool> {
    // NULL is ambiguous history, never account data. The original positive
    // scope survives repository deletion and must still resolve to current Read.
    let Some(id) = scope_repository_id.filter(|id| *id >= 0) else {
        return Ok(false);
    };
    if id == 0 {
        return Ok(true);
    }
    let repo: Option<(String, Option<i64>, Option<i64>)> = sqlx::query_as(
        "SELECT owner_kind,owner_user_id,owner_org_id FROM repositories WHERE id=?1",
    )
    .bind(id)
    .fetch_optional(&mut *connection)
    .await?;
    let Some((kind, owner, org)) = repo else {
        return Ok(false);
    };
    let personal = match (kind.as_str(), owner, org) {
        ("user", Some(owner), None) => {
            let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM users WHERE id=?1)")
                .bind(owner)
                .fetch_one(&mut *connection)
                .await?;
            if !exists {
                return Ok(false);
            }
            true
        }
        ("org", None, Some(org)) => {
            let exists: bool =
                sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM organizations WHERE id=?1)")
                    .bind(org)
                    .fetch_one(&mut *connection)
                    .await?;
            if !exists {
                return Ok(false);
            }
            false
        }
        _ => return Ok(false),
    };
    // Canonical Read includes archived/suspended-governance reads and the
    // persisted private-fork visibility floor; write authority is not Read.
    let access = super::super::repositories::access::user_repo_access_in_transaction(
        connection, user_id, id,
    )
    .await?;
    Ok(access.can_read() || (personal && admin))
}

/// Decides one repository for this transaction, the first time a query reaches it.
async fn scope_readable(
    connection: &mut sqlx::SqliteConnection,
    user_id: i64,
    admin: bool,
    scopes: &mut NotificationScopes,
    scope_repository_id: Option<i64>,
) -> Result<bool> {
    if let Some(readable) = scopes.readable.get(&scope_repository_id) {
        return Ok(*readable);
    }
    let readable =
        notification_scope_readable(connection, user_id, admin, scope_repository_id).await?;
    scopes.readable.insert(scope_repository_id, readable);
    Ok(readable)
}

// Keep SQLite's exact persisted ordering value, not a re-encoded timestamp.
type NotificationCandidate = (i64, Option<i64>, String);

async fn notification_scopes(
    connection: &mut sqlx::SqliteConnection,
    user_id: i64,
) -> Result<NotificationScopes> {
    Ok(NotificationScopes {
        high_water: notification_high_water(connection, user_id).await?,
        readable: std::collections::BTreeMap::new(),
    })
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

async fn notification_candidates(
    connection: &mut sqlx::SqliteConnection,
    user_id: i64,
    high_water: i64,
    unread_only: bool,
    unreadable: Vec<i64>,
    cursor: Option<&NotificationCandidate>,
) -> Result<Vec<NotificationCandidate>> {
    Ok(sqlx::query_as(
        "SELECT id,scope_repository_id,created_at FROM notifications
         WHERE user_id=?1 AND id <= ?2 AND (NOT ?3 OR read_at IS NULL)
           AND scope_repository_id IS NOT NULL
           AND scope_repository_id NOT IN (SELECT value FROM json_each(?4))
           AND (?5 IS NULL OR (created_at,id) < (?5,?6))
         ORDER BY created_at DESC,id DESC LIMIT 100",
    )
    .bind(user_id)
    .bind(high_water)
    .bind(unread_only)
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
    let Some(admin) = notification_recipient(&mut tx, user_id).await? else {
        tx.commit().await?;
        return Ok(Vec::new());
    };
    let mut scopes = notification_scopes(&mut tx, user_id).await?;
    let (notifications, _) = notification_list(
        &mut tx,
        user_id,
        admin,
        &mut scopes,
        limit,
        unread_only,
        None,
    )
    .await?;
    tx.commit().await?;
    Ok(notifications)
}

async fn notification_list(
    connection: &mut sqlx::SqliteConnection,
    user_id: i64,
    admin: bool,
    scopes: &mut NotificationScopes,
    limit: i64,
    unread_only: bool,
    before: Option<NotificationCursor>,
) -> Result<(Vec<Notification>, Option<NotificationCursor>)> {
    let limit = limit.clamp(1, 100) as usize;
    let mut cursor = before.map(|c| (c.id, None, c.created_at));
    let mut last_visible = None;
    let mut notifications = Vec::with_capacity(limit);
    loop {
        let page = notification_candidates(
            connection,
            user_id,
            scopes.high_water,
            unread_only,
            scopes.unreadable(),
            cursor.as_ref(),
        )
        .await?;
        if page.is_empty() {
            break;
        }
        for candidate in page {
            if !scope_readable(connection, user_id, admin, scopes, candidate.1).await? {
                cursor = Some(candidate);
                continue;
            }
            if notifications.len() == limit {
                return Ok((notifications, last_visible));
            }
            if let Some(notification) = sqlx::query_as::<_, Notification>(
                "SELECT * FROM notifications WHERE id=?1 AND user_id=?2 AND scope_repository_id IS ?3",
            ).bind(candidate.0).bind(user_id).bind(candidate.1).fetch_optional(&mut *connection).await? {
                last_visible = Some(NotificationCursor { created_at: candidate.2.clone(), id: candidate.0 });
                notifications.push(notification);
            }
            cursor = Some(candidate);
        }
    }
    Ok((notifications, None))
}

/// Unread rows grouped by repository, in id order with NULL last. Each group needs one
/// authorization, however many notifications it holds.
async fn unread_groups(
    connection: &mut sqlx::SqliteConnection,
    user_id: i64,
    high_water: i64,
) -> Result<Vec<(Option<i64>, i64)>> {
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

async fn unread_notification_count(
    connection: &mut sqlx::SqliteConnection,
    user_id: i64,
    admin: bool,
    scopes: &mut NotificationScopes,
) -> Result<i64> {
    let mut count = 0;
    for (scope, unread) in unread_groups(connection, user_id, scopes.high_water).await? {
        if scope_readable(connection, user_id, admin, scopes, scope).await? {
            count += unread;
        }
    }
    Ok(count)
}

pub async fn count_unread_notifications(pool: &SqlitePool, user_id: i64) -> Result<i64> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let count = match notification_recipient(&mut tx, user_id).await? {
        Some(admin) => {
            let mut scopes = notification_scopes(&mut tx, user_id).await?;
            unread_notification_count(&mut tx, user_id, admin, &mut scopes).await?
        }
        None => 0,
    };
    tx.commit().await?;
    Ok(count)
}

pub async fn notification_delivery(
    pool: &SqlitePool,
    session_id: &str,
    user_id: i64,
    notification_id: Option<i64>,
) -> Result<Option<crate::backend::db::notifications::NotificationDelivery>> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let Some(admin) = notification_recipient(&mut tx, user_id).await? else {
        return Ok(None);
    };
    let expiry: Option<String> = sqlx::query_scalar(
        "SELECT expires_at FROM sessions WHERE id=?1 AND user_id=?2 AND expires_at>strftime('%Y-%m-%dT%H:%M:%f000Z','now')",
    ).bind(session_id).bind(user_id).fetch_optional(&mut *tx).await?;
    let Some(expiry) = expiry else {
        return Ok(None);
    };
    let mut scopes = notification_scopes(&mut tx, user_id).await?;
    let mut notification = None;
    if let Some(id) = notification_id {
        let scope: Option<Option<i64>> = sqlx::query_scalar(
            "SELECT scope_repository_id FROM notifications WHERE id=?1 AND user_id=?2",
        )
        .bind(id)
        .bind(user_id)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(scope) = scope {
            if scope_readable(&mut tx, user_id, admin, &mut scopes, scope).await? {
                notification = sqlx::query_as("SELECT * FROM notifications WHERE id=?1 AND user_id=?2 AND scope_repository_id IS ?3")
                    .bind(id).bind(user_id).bind(scope).fetch_optional(&mut *tx).await?;
            }
        }
    }
    let unread_count = unread_notification_count(&mut tx, user_id, admin, &mut scopes).await?;
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
    let Some(admin) = notification_recipient(&mut tx, user_id).await? else {
        tx.commit().await?;
        return Ok(None);
    };
    let mut scopes = notification_scopes(&mut tx, user_id).await?;
    let (notification, _) =
        notification_read(&mut tx, user_id, admin, &mut scopes, notification_id).await?;
    tx.commit().await?;
    Ok(notification)
}

async fn notification_read(
    connection: &mut sqlx::SqliteConnection,
    user_id: i64,
    admin: bool,
    scopes: &mut NotificationScopes,
    notification_id: i64,
) -> Result<(Option<Notification>, u64)> {
    let scope: Option<(Option<i64>, bool)> = sqlx::query_as(
        "SELECT scope_repository_id,read_at IS NULL FROM notifications WHERE id=?1 AND user_id=?2",
    )
    .bind(notification_id)
    .bind(user_id)
    .fetch_optional(&mut *connection)
    .await?;
    if let Some((scope_repository_id, unread)) = scope {
        if scope_readable(connection, user_id, admin, scopes, scope_repository_id).await? {
            let notification = sqlx::query_as::<_, Notification>(
                "UPDATE notifications SET read_at=COALESCE(read_at,strftime('%Y-%m-%dT%H:%M:%f000Z','now')) WHERE id=?1 AND user_id=?2 AND scope_repository_id IS ?3 RETURNING *",
            ).bind(notification_id).bind(user_id).bind(scope_repository_id).fetch_optional(connection).await?;
            let updated = u64::from(unread && notification.is_some());
            return Ok((notification, updated));
        }
    }
    Ok((None, 0))
}

pub async fn mark_all_notifications_read(pool: &SqlitePool, user_id: i64) -> Result<u64> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let Some(admin) = notification_recipient(&mut tx, user_id).await? else {
        tx.commit().await?;
        return Ok(0);
    };
    let mut scopes = notification_scopes(&mut tx, user_id).await?;
    let count = notification_read_all(&mut tx, user_id, admin, &mut scopes).await?;
    tx.commit().await?;
    Ok(count)
}

async fn notification_read_all(
    connection: &mut sqlx::SqliteConnection,
    user_id: i64,
    admin: bool,
    scopes: &mut NotificationScopes,
) -> Result<u64> {
    let mut count = 0;
    for (scope, _) in unread_groups(connection, user_id, scopes.high_water).await? {
        if scope_readable(connection, user_id, admin, scopes, scope).await? {
            count += sqlx::query(
                "UPDATE notifications SET read_at=strftime('%Y-%m-%dT%H:%M:%f000Z','now') WHERE id<=?1 AND user_id=?2 AND read_at IS NULL AND scope_repository_id IS ?3",
            ).bind(scopes.high_water).bind(user_id).bind(scope).execute(&mut *connection).await?.rows_affected();
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
    let Some(admin) = notification_recipient(&mut tx, user_id).await? else {
        tx.rollback().await?;
        return Ok(None);
    };
    let expiry: Option<String> = sqlx::query_scalar(
        "SELECT expires_at FROM sessions WHERE id=?1 AND user_id=?2 AND expires_at>strftime('%Y-%m-%dT%H:%M:%f000Z','now')",
    ).bind(session_id).bind(user_id).fetch_optional(&mut *tx).await?;
    let Some(expiry) = expiry else {
        tx.rollback().await?;
        return Ok(None);
    };
    let mut scopes = notification_scopes(&mut tx, user_id).await?;
    let mut next_cursor = None;
    let (notifications, updated) = match action {
        NotificationInboxAction::List {
            limit,
            unread_only,
            before,
        } => {
            let (notifications, next) = notification_list(
                &mut tx,
                user_id,
                admin,
                &mut scopes,
                limit,
                unread_only,
                before,
            )
            .await?;
            next_cursor = next;
            (notifications, 0)
        }
        NotificationInboxAction::Read { notification_id } => {
            let (notification, updated) =
                notification_read(&mut tx, user_id, admin, &mut scopes, notification_id).await?;
            (notification.into_iter().collect(), updated)
        }
        NotificationInboxAction::ReadAll => (
            Vec::new(),
            notification_read_all(&mut tx, user_id, admin, &mut scopes).await?,
        ),
    };
    let unread_count = unread_notification_count(&mut tx, user_id, admin, &mut scopes).await?;
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
        updated,
    }))
}
