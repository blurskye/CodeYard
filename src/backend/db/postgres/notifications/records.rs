use crate::backend::db::models::Notification;
use crate::backend::db::notifications::records::{
    NotificationCounts, NotificationCursor, NotificationFilter, NotificationMark,
    NotificationScopes,
};
use crate::backend::db::notifications::{
    NewNotification, NotificationInbox, NotificationInboxAction,
};
use crate::backend::db::postgres::repositories::access::USER_REPO_ACCESS_SQL;
use crate::backend::db::postgres::users::transitions::lock_membership_shared;
use anyhow::Result;
use sqlx::PgPool;

pub async fn create_notification(
    pool: &PgPool,
    input: NewNotification<'_>,
) -> Result<Notification> {
    use crate::backend::db::notifications::records::NotificationSuppressed;
    let mut tx = pool.begin().await?;
    lock_membership_shared(&mut tx).await?;
    if input.kind == "user_followed" {
        let _: Option<i64> =
            sqlx::query_scalar("SELECT id FROM users WHERE id=$1 FOR NO KEY UPDATE")
                .bind(input.user_id)
                .fetch_optional(&mut *tx)
                .await?;
        let recent: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM notifications WHERE user_id=$1 AND actor_id=$2 AND kind='user_followed' AND created_at>clock_timestamp()-INTERVAL '1 day')")
            .bind(input.user_id).bind(input.actor_id).fetch_one(&mut *tx).await?;
        anyhow::ensure!(!recent, NotificationSuppressed);
    }
    if matches!(input.kind, "issue_comment" | "mention") {
        if let Some(comment_id) = input
            .metadata
            .get("comment_id")
            .and_then(serde_json::Value::as_i64)
        {
            sqlx::query("SELECT id FROM repositories WHERE id=$1 FOR SHARE")
                .bind(input.repo_id)
                .fetch_optional(&mut *tx)
                .await?;
            let comment: Option<i64> = sqlx::query_scalar(
                "SELECT c.id FROM issue_comments c JOIN issues i ON i.id=c.issue_id
                 WHERE c.id=$1 AND i.repo_id=$2 FOR SHARE OF c",
            )
            .bind(comment_id)
            .bind(input.repo_id)
            .fetch_optional(&mut *tx)
            .await?;
            anyhow::ensure!(comment.is_some(), NotificationSuppressed);
        }
    }
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
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
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
/// commits meanwhile waits for this read. The original positive scope survives
/// repository deletion and must still resolve to current Read.
async fn lock_notification_scope_readable(
    connection: &mut sqlx::PgConnection,
    user_id: i64,
    scope_repository_id: i64,
) -> Result<bool> {
    if scope_repository_id == 0 {
        return Ok(true);
    }
    let Some(authority) = super::super::repositories::access::lock_user_repo_access_in_transaction(
        connection,
        user_id,
        scope_repository_id,
        super::super::repositories::access::RepositoryRowLock::Shared,
        true,
    )
    .await?
    else {
        return Ok(false);
    };
    Ok(authority.access.can_read() || (!authority.organization && authority.is_admin))
}

type NotificationCandidate = (i64, i64, chrono::DateTime<chrono::Utc>);

/// Decides one repository for this transaction, the first time a query reaches it. The
/// authority check locks the repository's sources before the repository itself (see
/// lock_fork_ancestors), so reads and fork deletions take a fork family in one order.
async fn scope_readable(
    connection: &mut sqlx::PgConnection,
    user_id: i64,
    scopes: &mut NotificationScopes,
    scope_repository_id: i64,
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

/// The filter as SQL. It binds $3 unread only, $4 the view, $5 the repository, $6 the kinds
/// as text[] and $7 the search text; each query names its other parameters itself.
const FILTER_SQL: &str = "(NOT $3 OR (read_at IS NULL AND done_at IS NULL))
  AND (($4::text='inbox' AND done_at IS NULL)
    OR ($4::text='saved' AND saved_at IS NOT NULL AND done_at IS NULL)
    OR ($4::text='done' AND done_at IS NOT NULL))
  AND ($5::bigint IS NULL OR scope_repository_id=$5)
  AND (cardinality($6::text[])=0 OR kind = ANY($6::text[]))
  AND ($7::text IS NULL OR strpos(lower(title),$7::text)>0 OR strpos(lower(metadata->>'subject'),$7::text)>0
    OR EXISTS (SELECT 1 FROM repositories r
               WHERE r.id=notifications.scope_repository_id AND strpos(lower(r.name),$7::text)>0))";

async fn notification_candidates(
    connection: &mut sqlx::PgConnection,
    user_id: i64,
    high_water: i64,
    filter: &NotificationFilter,
    search: Option<&str>,
    unreadable: Vec<i64>,
    cursor: Option<&NotificationCandidate>,
) -> Result<Vec<NotificationCandidate>> {
    let sql = format!(
        "SELECT id,scope_repository_id,created_at FROM notifications
         WHERE user_id=$1 AND id <= $2 AND {FILTER_SQL}
           AND NOT (scope_repository_id = ANY($8))
           AND ($9::timestamptz IS NULL OR (created_at,id) < ($9,$10))
         ORDER BY created_at DESC,id DESC LIMIT 100"
    );
    Ok(sqlx::query_as(&sql)
        .bind(user_id)
        .bind(high_water)
        .bind(filter.unread_only)
        .bind(filter.view.as_str())
        .bind(filter.repository_id)
        .bind(filter.kinds.clone())
        .bind(search)
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
    let (notifications, _) = notification_list(
        &mut tx,
        user_id,
        &mut scopes,
        &NotificationFilter::inbox(unread_only),
        limit,
        None,
    )
    .await?;
    tx.commit().await?;
    Ok(notifications)
}

async fn notification_list(
    connection: &mut sqlx::PgConnection,
    user_id: i64,
    scopes: &mut NotificationScopes,
    filter: &NotificationFilter,
    limit: i64,
    before: Option<NotificationCursor>,
) -> Result<(Vec<Notification>, Option<NotificationCursor>)> {
    let limit = limit.clamp(1, 100) as usize;
    let search = filter.search_text();
    // The page position is (created_at, id); the scope slot is never read.
    let mut cursor = before
        .map(|c| {
            chrono::DateTime::parse_from_rfc3339(&c.created_at)
                .map(|date| (c.id, 0, date.with_timezone(&chrono::Utc)))
        })
        .transpose()?;
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
        for candidate in page {
            if !scope_readable(connection, user_id, scopes, candidate.1).await? {
                cursor = Some(candidate);
                continue;
            }
            if notifications.len() == limit {
                return Ok((notifications, last_visible));
            }
            if let Some(notification) = sqlx::query_as::<_, Notification>(
                "SELECT * FROM notifications WHERE id=$1 AND user_id=$2 AND scope_repository_id = $3",
            ).bind(candidate.0).bind(user_id).bind(candidate.1).fetch_optional(&mut *connection).await? {
                last_visible = Some(NotificationCursor { created_at: candidate.2.to_rfc3339(), id: candidate.0 });
                notifications.push(notification);
            }
            cursor = Some(candidate);
        }
    }
    Ok((notifications, None))
}

/// Unread rows grouped by repository, ordered by repository ID. Each group needs one
/// authorization, however many notifications it holds.
async fn unread_groups(
    connection: &mut sqlx::PgConnection,
    user_id: i64,
    high_water: i64,
) -> Result<Vec<(i64, i64)>> {
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

/// Unread inbox rows grouped by repository, ordered by repository ID. The unread badge
/// authorizes only the repositories that still have one.
async fn unread_inbox_groups(
    connection: &mut sqlx::PgConnection,
    user_id: i64,
    high_water: i64,
) -> Result<Vec<(i64, i64)>> {
    Ok(sqlx::query_as(
        "SELECT scope_repository_id,COUNT(*) FROM notifications
         WHERE user_id=$1 AND id<=$2 AND read_at IS NULL AND done_at IS NULL
         GROUP BY scope_repository_id ORDER BY scope_repository_id",
    )
    .bind(user_id)
    .bind(high_water)
    .fetch_all(connection)
    .await?)
}

/// Unread notifications in the inbox. Done rows are not counted.
async fn notification_unread_count(
    connection: &mut sqlx::PgConnection,
    user_id: i64,
    scopes: &mut NotificationScopes,
) -> Result<i64> {
    let mut count = 0;
    for (scope, unread) in unread_inbox_groups(connection, user_id, scopes.high_water).await? {
        if scope_readable(connection, user_id, scopes, scope).await? {
            count += unread;
        }
    }
    Ok(count)
}

/// Counts per repository scope, ordered by repository ID: inbox, saved and done rows. The
/// grouping reads no authority; `notification_counts` decides each group.
async fn notification_scope_counts(
    connection: &mut sqlx::PgConnection,
    user_id: i64,
    high_water: i64,
) -> Result<Vec<(i64, i64, i64, i64)>> {
    Ok(sqlx::query_as(
        "SELECT scope_repository_id,
                COUNT(*) FILTER (WHERE done_at IS NULL),
                COUNT(*) FILTER (WHERE done_at IS NULL AND saved_at IS NOT NULL),
                COUNT(*) FILTER (WHERE done_at IS NOT NULL)
         FROM notifications WHERE user_id=$1 AND id<=$2
         GROUP BY scope_repository_id ORDER BY scope_repository_id",
    )
    .bind(user_id)
    .bind(high_water)
    .fetch_all(connection)
    .await?)
}

/// The tab counts and the repositories with notifications, over what the recipient can read.
async fn notification_counts(
    connection: &mut sqlx::PgConnection,
    user_id: i64,
    scopes: &mut NotificationScopes,
) -> Result<NotificationCounts> {
    let mut counts = NotificationCounts::default();
    let mut repositories = Vec::new();
    for (scope, inbox, saved, done) in
        notification_scope_counts(connection, user_id, scopes.high_water).await?
    {
        if scope_readable(connection, user_id, scopes, scope).await? {
            counts.views.inbox += inbox;
            counts.views.saved += saved;
            counts.views.done += done;
            if scope > 0 {
                repositories.push(scope);
            }
        }
    }
    // Named after every decision above, under the row locks those decisions took, so a
    // repository revoked meanwhile is never named.
    let names: std::collections::BTreeMap<i64, String> = sqlx::query_as::<_, (i64, String)>(
        "SELECT r.id, COALESCE(ou.username, oo.name) || '/' || r.name
         FROM repositories r
         LEFT JOIN users ou ON r.owner_kind='user' AND ou.id=r.owner_user_id
         LEFT JOIN organizations oo ON r.owner_kind='org' AND oo.id=r.owner_org_id
         WHERE r.id = ANY($1)",
    )
    .bind(&repositories)
    .fetch_all(connection)
    .await?
    .into_iter()
    .collect();
    counts.repositories = repositories
        .into_iter()
        .filter_map(|scope| names.get(&scope).map(|name| (scope, name.clone())))
        .collect();
    Ok(counts)
}

pub async fn count_unread_notifications(pool: &PgPool, user_id: i64) -> Result<i64> {
    let mut tx = pool.begin().await?;
    let count = match notification_recipient(&mut tx, user_id).await? {
        Some(_) => {
            let mut scopes = notification_scopes(&mut tx, user_id).await?;
            notification_unread_count(&mut tx, user_id, &mut scopes).await?
        }
        None => 0,
    };
    tx.commit().await?;
    Ok(count)
}

/// Whether the recipient can read the repository a notification names, as the list decides it:
/// the owner must exist and be well formed, and then the recipient needs Read or is a site admin
/// over a personal repository. See the SQLite module for why this is not `readable_condition`.
fn scope_readable_condition() -> String {
    format!(
        "((r.owner_kind='user' AND r.owner_user_id IS NOT NULL AND r.owner_org_id IS NULL
            AND EXISTS (SELECT 1 FROM users ow WHERE ow.id=r.owner_user_id))
          OR (r.owner_kind='org' AND r.owner_user_id IS NULL AND r.owner_org_id IS NOT NULL
            AND EXISTS (SELECT 1 FROM organizations og WHERE og.id=r.owner_org_id)))
         AND (({USER_REPO_ACCESS_SQL}) >= 1 OR (r.owner_kind='user' AND u.is_admin))"
    )
}

/// The unread count the page header shows: unread inbox notifications the recipient can read.
/// It grants nothing and only displays a number, so one statement decides every repository at
/// once and takes no row locks. The list takes them, because its rows are returned.
pub async fn count_header_unread(pool: &PgPool, user_id: i64) -> Result<i64> {
    let readable = scope_readable_condition();
    Ok(sqlx::query_scalar::<_, i64>(&format!(
        "SELECT COUNT(*) FROM notifications
         WHERE user_id=$1 AND read_at IS NULL AND done_at IS NULL
           AND (scope_repository_id=0 OR scope_repository_id IN (
               SELECT r.id FROM repositories r
               JOIN users u ON u.id=$1 AND NOT u.is_banned AND NOT u.must_change_password
               WHERE r.id IN (SELECT scope_repository_id FROM notifications
                              WHERE user_id=$1 AND read_at IS NULL AND done_at IS NULL
                                AND scope_repository_id>0)
                 AND {readable}))"
    ))
    .bind(user_id)
    .fetch_one(pool)
    .await?)
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
        let scope: Option<i64> = sqlx::query_scalar(
            "SELECT scope_repository_id FROM notifications WHERE id=$1 AND user_id=$2",
        )
        .bind(id)
        .bind(user_id)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(scope) = scope {
            if scope_readable(&mut tx, user_id, &mut scopes, scope).await? {
                notification = sqlx::query_as("SELECT * FROM notifications WHERE id=$1 AND user_id=$2 AND scope_repository_id = $3")
                    .bind(id).bind(user_id).bind(scope).fetch_optional(&mut *tx).await?;
            }
        }
    }
    let unread_count = notification_unread_count(&mut tx, user_id, &mut scopes).await?;
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
    let scope: Option<i64> = sqlx::query_scalar(
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
        "SELECT read_at IS NULL FROM notifications WHERE id=$1 AND user_id=$2 AND scope_repository_id = $3 FOR UPDATE",
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
        "UPDATE notifications SET read_at=COALESCE(read_at,NOW()) WHERE id=$1 AND user_id=$2 AND scope_repository_id = $3 RETURNING *",
    ).bind(notification_id).bind(user_id).bind(scope_repository_id).fetch_optional(connection).await?;
    let updated = u64::from(unread && notification.is_some());
    Ok((notification, updated))
}

/// Sets or clears a mark on a notification the recipient can read. Setting a mark that
/// is already set keeps its first time, so the change count is zero.
async fn notification_mark(
    connection: &mut sqlx::PgConnection,
    user_id: i64,
    scopes: &mut NotificationScopes,
    notification_id: i64,
    mark: NotificationMark,
    on: bool,
) -> Result<(Option<Notification>, u64)> {
    let column = mark.column();
    // Same lock order as notification_read: the scope is authorized before the row is locked.
    let scope: Option<i64> = sqlx::query_scalar(
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
    let was_set: Option<bool> = sqlx::query_scalar(&format!(
        "SELECT {column} IS NOT NULL FROM notifications WHERE id=$1 AND user_id=$2 AND scope_repository_id=$3 FOR UPDATE"
    ))
    .bind(notification_id)
    .bind(user_id)
    .bind(scope_repository_id)
    .fetch_optional(&mut *connection)
    .await?;
    let Some(was_set) = was_set else {
        return Ok((None, 0));
    };
    let notification = sqlx::query_as::<_, Notification>(&format!(
        "UPDATE notifications SET {column}=CASE WHEN $3::boolean THEN COALESCE({column},NOW()) ELSE NULL END
         WHERE id=$1 AND user_id=$2 AND scope_repository_id=$4 RETURNING *"
    ))
    .bind(notification_id)
    .bind(user_id)
    .bind(on)
    .bind(scope_repository_id)
    .fetch_optional(connection)
    .await?;
    let updated = u64::from(was_set != on && notification.is_some());
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
                "UPDATE notifications SET read_at=NOW() WHERE id<=$1 AND user_id=$2 AND read_at IS NULL AND scope_repository_id = $3",
            ).bind(scopes.high_water).bind(user_id).bind(scope).execute(&mut *connection).await?.rows_affected();
        }
    }
    Ok(count)
}

/// Marks every notification of the filter as done, in each repository the recipient can
/// read. Hidden repositories are skipped, so their history is never changed.
async fn notification_done_all(
    connection: &mut sqlx::PgConnection,
    user_id: i64,
    scopes: &mut NotificationScopes,
    filter: &NotificationFilter,
) -> Result<u64> {
    let sql = format!(
        "UPDATE notifications SET done_at=NOW()
         WHERE user_id=$1 AND id<=$2 AND done_at IS NULL AND scope_repository_id=$8 AND {FILTER_SQL}"
    );
    let search = filter.search_text();
    let mut count = 0;
    for (scope, _, _, _) in
        notification_scope_counts(connection, user_id, scopes.high_water).await?
    {
        if scope_readable(connection, user_id, scopes, scope).await? {
            count += sqlx::query(&sql)
                .bind(user_id)
                .bind(scopes.high_water)
                .bind(filter.unread_only)
                .bind(filter.view.as_str())
                .bind(filter.repository_id)
                .bind(filter.kinds.clone())
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
    let wants_counts = matches!(action, NotificationInboxAction::List { .. });
    let (notifications, updated) = match action {
        NotificationInboxAction::List {
            filter,
            limit,
            before,
        } => {
            let (notifications, next) =
                notification_list(&mut tx, user_id, &mut scopes, &filter, limit, before).await?;
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
        NotificationInboxAction::Save {
            notification_id,
            saved,
        } => {
            let (notification, updated) = notification_mark(
                &mut tx,
                user_id,
                &mut scopes,
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
                &mut scopes,
                notification_id,
                NotificationMark::Done,
                done,
            )
            .await?;
            (notification.into_iter().collect(), updated)
        }
        NotificationInboxAction::DoneAll { filter } => (
            Vec::new(),
            notification_done_all(&mut tx, user_id, &mut scopes, &filter).await?,
        ),
    };
    let unread_count = notification_unread_count(&mut tx, user_id, &mut scopes).await?;
    let counts = if wants_counts {
        Some(notification_counts(&mut tx, user_id, &mut scopes).await?)
    } else {
        None
    };
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
        counts,
        updated,
    }))
}
