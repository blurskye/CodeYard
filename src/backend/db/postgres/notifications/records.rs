use crate::backend::db::models::Notification;
use crate::backend::db::notifications::records::{NotificationCursor, NotificationScopes};
use crate::backend::db::notifications::{
    NewNotification, NotificationInbox, NotificationInboxAction,
};
use crate::backend::db::postgres::users::transitions::lock_membership_shared;
use anyhow::Result;
use sqlx::PgPool;

pub async fn create_notification(
    pool: &PgPool,
    input: NewNotification<'_>,
) -> Result<Notification> {
    let notification = sqlx::query_as::<_, Notification>(
        r#"
        INSERT INTO notifications (user_id, actor_id, repo_id, kind, title, body, link, metadata, scope_repository_id)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, COALESCE($3,0))
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
    .bind(input.metadata)
    .fetch_one(pool)
    .await?;

    Ok(notification)
}

// Every notification transaction starts here, so the shared membership lock comes first.
// The recipient row is locked FOR SHARE so account changes wait for this transaction.
// Never load a payload before its repository scope has passed Read policy.
async fn notification_recipient(
    connection: &mut sqlx::PgConnection,
    user_id: i64,
) -> Result<Option<bool>> {
    lock_membership_shared(&mut *connection).await?;
    Ok(sqlx::query_scalar(
        "SELECT is_admin FROM users WHERE id=$1 AND NOT is_banned AND NOT must_change_password FOR SHARE",
    ).bind(user_id).fetch_optional(connection).await?)
}

/// The authority rows stay locked until the transaction commits, so a revocation that
/// commits meanwhile waits for this read. NULL is ambiguous history, never account data.
/// The original positive scope survives repository deletion and must still resolve to
/// current Read.
async fn lock_notification_scope_readable(
    connection: &mut sqlx::PgConnection,
    user_id: i64,
    scope_repository_id: Option<i64>,
) -> Result<bool> {
    let Some(id) = scope_repository_id.filter(|id| *id >= 0) else {
        return Ok(false);
    };
    if id == 0 {
        return Ok(true);
    }
    let Some(authority) = super::super::repositories::access::lock_user_repo_access_in_transaction(
        connection,
        user_id,
        id,
        super::super::repositories::access::RepositoryRowLock::Shared,
        true,
    )
    .await?
    else {
        return Ok(false);
    };
    Ok(authority.access.can_read() || (!authority.organization && authority.is_admin))
}

type NotificationCandidate = (i64, Option<i64>, chrono::DateTime<chrono::Utc>);

/// Decides one repository for this transaction, the first time a query reaches it. The
/// authority check locks the repository's sources before the repository itself (see
/// lock_fork_ancestors), so reads and fork deletions take a fork family in one order.
async fn scope_readable(
    connection: &mut sqlx::PgConnection,
    user_id: i64,
    scopes: &mut NotificationScopes,
    scope_repository_id: Option<i64>,
) -> Result<bool> {
    if let Some(readable) = scopes.readable.get(&scope_repository_id) {
        return Ok(*readable);
    }
    let readable =
        lock_notification_scope_readable(connection, user_id, scope_repository_id).await?;
    scopes.readable.insert(scope_repository_id, readable);
    Ok(readable)
}

async fn notification_scopes(
    connection: &mut sqlx::PgConnection,
    user_id: i64,
) -> Result<NotificationScopes> {
    Ok(NotificationScopes {
        high_water: notification_high_water(connection, user_id).await?,
        readable: std::collections::BTreeMap::new(),
    })
}

async fn notification_high_water(connection: &mut sqlx::PgConnection, user_id: i64) -> Result<i64> {
    Ok(
        sqlx::query_scalar("SELECT COALESCE(MAX(id),0) FROM notifications WHERE user_id=$1")
            .bind(user_id)
            .fetch_one(connection)
            .await?,
    )
}

async fn notification_candidates(
    connection: &mut sqlx::PgConnection,
    user_id: i64,
    high_water: i64,
    unread_only: bool,
    unreadable: Vec<i64>,
    cursor: Option<&NotificationCandidate>,
) -> Result<Vec<NotificationCandidate>> {
    Ok(sqlx::query_as(
        "SELECT id,scope_repository_id,created_at FROM notifications
         WHERE user_id=$1 AND id <= $2 AND (NOT $3 OR read_at IS NULL)
           AND scope_repository_id IS NOT NULL AND NOT (scope_repository_id = ANY($4))
           AND ($5::timestamptz IS NULL OR (created_at,id) < ($5,$6))
         ORDER BY created_at DESC,id DESC LIMIT 100",
    )
    .bind(user_id)
    .bind(high_water)
    .bind(unread_only)
    .bind(unreadable)
    .bind(cursor.map(|c| c.2))
    .bind(cursor.map(|c| c.0))
    .fetch_all(connection)
    .await?)
}

pub async fn list_notifications_for_user(
    pool: &PgPool,
    user_id: i64,
    limit: i64,
    unread_only: bool,
) -> Result<Vec<Notification>> {
    let mut tx = pool.begin().await?;
    let Some(_) = notification_recipient(&mut tx, user_id).await? else {
        tx.commit().await?;
        return Ok(Vec::new());
    };
    let mut scopes = notification_scopes(&mut tx, user_id).await?;
    let (notifications, _) =
        notification_list(&mut tx, user_id, &mut scopes, limit, unread_only, None).await?;
    tx.commit().await?;
    Ok(notifications)
}

async fn notification_list(
    connection: &mut sqlx::PgConnection,
    user_id: i64,
    scopes: &mut NotificationScopes,
    limit: i64,
    unread_only: bool,
    before: Option<NotificationCursor>,
) -> Result<(Vec<Notification>, Option<NotificationCursor>)> {
    let limit = limit.clamp(1, 100) as usize;
    let mut cursor = before
        .map(|c| {
            chrono::DateTime::parse_from_rfc3339(&c.created_at)
                .map(|date| (c.id, None, date.with_timezone(&chrono::Utc)))
        })
        .transpose()?;
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
            if !scope_readable(connection, user_id, scopes, candidate.1).await? {
                cursor = Some(candidate);
                continue;
            }
            if notifications.len() == limit {
                return Ok((notifications, last_visible));
            }
            if let Some(notification) = sqlx::query_as::<_, Notification>(
                "SELECT * FROM notifications WHERE id=$1 AND user_id=$2 AND scope_repository_id IS NOT DISTINCT FROM $3",
            ).bind(candidate.0).bind(user_id).bind(candidate.1).fetch_optional(&mut *connection).await? {
                last_visible = Some(NotificationCursor { created_at: candidate.2.to_rfc3339(), id: candidate.0 });
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
    connection: &mut sqlx::PgConnection,
    user_id: i64,
    high_water: i64,
) -> Result<Vec<(Option<i64>, i64)>> {
    Ok(sqlx::query_as(
        "SELECT scope_repository_id,COUNT(*) FROM notifications
         WHERE user_id=$1 AND id<=$2 AND read_at IS NULL
         GROUP BY scope_repository_id ORDER BY scope_repository_id",
    )
    .bind(user_id)
    .bind(high_water)
    .fetch_all(connection)
    .await?)
}

async fn unread_notification_count(
    connection: &mut sqlx::PgConnection,
    user_id: i64,
    scopes: &mut NotificationScopes,
) -> Result<i64> {
    let mut count = 0;
    for (scope, unread) in unread_groups(connection, user_id, scopes.high_water).await? {
        if scope_readable(connection, user_id, scopes, scope).await? {
            count += unread;
        }
    }
    Ok(count)
}

pub async fn count_unread_notifications(pool: &PgPool, user_id: i64) -> Result<i64> {
    let mut tx = pool.begin().await?;
    let count = match notification_recipient(&mut tx, user_id).await? {
        Some(_) => {
            let mut scopes = notification_scopes(&mut tx, user_id).await?;
            unread_notification_count(&mut tx, user_id, &mut scopes).await?
        }
        None => 0,
    };
    tx.commit().await?;
    Ok(count)
}

pub async fn notification_delivery(
    pool: &PgPool,
    session_id: &str,
    user_id: i64,
    notification_id: Option<i64>,
) -> Result<Option<crate::backend::db::notifications::NotificationDelivery>> {
    let mut tx = pool.begin().await?;
    let Some(_) = notification_recipient(&mut tx, user_id).await? else {
        return Ok(None);
    };
    let expiry: Option<chrono::DateTime<chrono::Utc>> = sqlx::query_scalar(
        "SELECT expires_at FROM sessions WHERE id=$1 AND user_id=$2 AND expires_at>clock_timestamp() FOR SHARE",
    ).bind(session_id).bind(user_id).fetch_optional(&mut *tx).await?;
    let Some(expiry) = expiry else {
        return Ok(None);
    };
    let mut scopes = notification_scopes(&mut tx, user_id).await?;
    let mut notification = None;
    if let Some(id) = notification_id {
        let scope: Option<Option<i64>> = sqlx::query_scalar(
            "SELECT scope_repository_id FROM notifications WHERE id=$1 AND user_id=$2",
        )
        .bind(id)
        .bind(user_id)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(scope) = scope {
            if scope_readable(&mut tx, user_id, &mut scopes, scope).await? {
                notification = sqlx::query_as("SELECT * FROM notifications WHERE id=$1 AND user_id=$2 AND scope_repository_id IS NOT DISTINCT FROM $3")
                    .bind(id).bind(user_id).bind(scope).fetch_optional(&mut *tx).await?;
            }
        }
    }
    let unread_count = unread_notification_count(&mut tx, user_id, &mut scopes).await?;
    let alive: bool = sqlx::query_scalar("SELECT $1::timestamptz>clock_timestamp()")
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
    pool: &PgPool,
    user_id: i64,
    notification_id: i64,
) -> Result<Option<Notification>> {
    let mut tx = pool.begin().await?;
    let Some(_) = notification_recipient(&mut tx, user_id).await? else {
        tx.commit().await?;
        return Ok(None);
    };
    let mut scopes = notification_scopes(&mut tx, user_id).await?;
    let (notification, _) =
        notification_read(&mut tx, user_id, &mut scopes, notification_id).await?;
    tx.commit().await?;
    Ok(notification)
}

async fn notification_read(
    connection: &mut sqlx::PgConnection,
    user_id: i64,
    scopes: &mut NotificationScopes,
    notification_id: i64,
) -> Result<(Option<Notification>, u64)> {
    // Read the scope without a notification lock. Deleting a repository takes
    // its row lock before changing notification FK rows, so lock/authorize the
    // repository before locking this notification for the update.
    let scope: Option<Option<i64>> = sqlx::query_scalar(
        "SELECT scope_repository_id FROM notifications WHERE id=$1 AND user_id=$2",
    )
    .bind(notification_id)
    .bind(user_id)
    .fetch_optional(&mut *connection)
    .await?;
    let Some(scope_repository_id) = scope else {
        return Ok((None, 0));
    };
    if !scope_readable(connection, user_id, scopes, scope_repository_id).await? {
        return Ok((None, 0));
    }
    // Serialize acknowledgments without upgrading a shared row lock; a second
    // reader observes the committed read_at after waiting for the first.
    let unread: Option<bool> = sqlx::query_scalar(
        "SELECT read_at IS NULL FROM notifications WHERE id=$1 AND user_id=$2 AND scope_repository_id IS NOT DISTINCT FROM $3 FOR UPDATE",
    )
    .bind(notification_id)
    .bind(user_id)
    .bind(scope_repository_id)
    .fetch_optional(&mut *connection)
    .await?;
    let Some(unread) = unread else {
        return Ok((None, 0));
    };
    let notification = sqlx::query_as::<_, Notification>(
        "UPDATE notifications SET read_at=COALESCE(read_at,NOW()) WHERE id=$1 AND user_id=$2 AND scope_repository_id IS NOT DISTINCT FROM $3 RETURNING *",
    ).bind(notification_id).bind(user_id).bind(scope_repository_id).fetch_optional(connection).await?;
    let updated = u64::from(unread && notification.is_some());
    Ok((notification, updated))
}

pub async fn mark_all_notifications_read(pool: &PgPool, user_id: i64) -> Result<u64> {
    let mut tx = pool.begin().await?;
    let Some(_) = notification_recipient(&mut tx, user_id).await? else {
        tx.commit().await?;
        return Ok(0);
    };
    let mut scopes = notification_scopes(&mut tx, user_id).await?;
    let count = notification_read_all(&mut tx, user_id, &mut scopes).await?;
    tx.commit().await?;
    Ok(count)
}

async fn notification_read_all(
    connection: &mut sqlx::PgConnection,
    user_id: i64,
    scopes: &mut NotificationScopes,
) -> Result<u64> {
    let mut count = 0;
    for (scope, _) in unread_groups(connection, user_id, scopes.high_water).await? {
        if scope_readable(connection, user_id, scopes, scope).await? {
            count += sqlx::query(
                "UPDATE notifications SET read_at=NOW() WHERE id<=$1 AND user_id=$2 AND read_at IS NULL AND scope_repository_id IS NOT DISTINCT FROM $3",
            ).bind(scopes.high_water).bind(user_id).bind(scope).execute(&mut *connection).await?.rows_affected();
        }
    }
    Ok(count)
}

pub async fn notification_inbox(
    pool: &PgPool,
    session_id: &str,
    user_id: i64,
    action: NotificationInboxAction,
) -> Result<Option<NotificationInbox>> {
    let mut tx = pool.begin().await?;
    // Lock order: shared membership, recipient, session, then each repository as the
    // action reaches it (its ancestors first), then notification rows.
    let Some(_) = notification_recipient(&mut tx, user_id).await? else {
        tx.rollback().await?;
        return Ok(None);
    };
    let expiry: Option<chrono::DateTime<chrono::Utc>> = sqlx::query_scalar(
        "SELECT expires_at FROM sessions WHERE id=$1 AND user_id=$2 AND expires_at>clock_timestamp() FOR SHARE",
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
            let (notifications, next) =
                notification_list(&mut tx, user_id, &mut scopes, limit, unread_only, before)
                    .await?;
            next_cursor = next;
            (notifications, 0)
        }
        NotificationInboxAction::Read { notification_id } => {
            let (notification, updated) =
                notification_read(&mut tx, user_id, &mut scopes, notification_id).await?;
            (notification.into_iter().collect(), updated)
        }
        NotificationInboxAction::ReadAll => (
            Vec::new(),
            notification_read_all(&mut tx, user_id, &mut scopes).await?,
        ),
    };
    let unread_count = unread_notification_count(&mut tx, user_id, &mut scopes).await?;
    let alive: bool = sqlx::query_scalar("SELECT $1::timestamptz>clock_timestamp()")
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
