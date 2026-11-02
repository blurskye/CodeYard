use super::*;
use crate::backend::db::{
    models::Repository,
    notifications::threads::{
        self as model, NotificationThread, ThreadAction, ThreadFilter, ThreadInbox,
        ThreadSubscription, ThreadSummary, ThreadTarget, ThreadView,
    },
};

pub(super) async fn ensure(
    connection: &mut sqlx::SqliteConnection,
    input: &NewNotification<'_>,
) -> Result<Option<i64>> {
    let Some((kind, key)) = model::subject(input) else {
        return Ok(None);
    };
    let id:i64=sqlx::query_scalar("INSERT INTO notification_threads(user_id,scope_repository_id,subject_type,subject_key) VALUES(?1,?2,?3,?4) ON CONFLICT(user_id,scope_repository_id,subject_type,subject_key) DO UPDATE SET subject_key=excluded.subject_key RETURNING id").bind(input.user_id).bind(input.repo_id).bind(kind).bind(key).fetch_one(&mut *connection).await?;
    sqlx::query("INSERT INTO notification_thread_subscriptions(thread_id) VALUES(?1) ON CONFLICT(thread_id) DO NOTHING").bind(id).execute(&mut *connection).await?;
    let (subscribed, ignored): (bool, bool) = sqlx::query_as(
        "SELECT subscribed,ignored FROM notification_thread_subscriptions WHERE thread_id=?1",
    )
    .bind(id)
    .fetch_one(connection)
    .await?;
    anyhow::ensure!(
        model::allows_delivery(subscribed, ignored, input.kind),
        crate::backend::db::notifications::records::NotificationSuppressed
    );
    Ok(Some(id))
}
pub(crate) async fn subscribers(
    pool: &SqlitePool,
    repo_id: i64,
    kind: &str,
    key: &str,
) -> Result<Vec<i64>> {
    let scope = super::scope_readable_condition();
    Ok(sqlx::query_scalar(&format!("SELECT t.user_id FROM notification_threads t JOIN notification_thread_subscriptions s ON s.thread_id=t.id JOIN users u ON u.id=t.user_id JOIN repositories r ON r.id=t.scope_repository_id WHERE t.scope_repository_id=?1 AND t.subject_type=?2 AND t.subject_key=?3 AND s.subscribed AND s.is_explicit AND NOT s.ignored AND NOT u.is_banned AND NOT u.must_change_password AND {scope} AND NOT EXISTS(SELECT 1 FROM watches w WHERE w.repo_id=?1 AND w.user_id=t.user_id AND w.level='ignoring') ORDER BY t.user_id")).bind(repo_id).bind(kind).bind(key).fetch_all(pool).await?)
}
pub(crate) async fn attach_existing(
    connection: &mut sqlx::SqliteConnection,
    id: i64,
) -> Result<bool> {
    let notification: Notification = sqlx::query_as("SELECT * FROM notifications WHERE id=?1")
        .bind(id)
        .fetch_one(&mut *connection)
        .await?;
    let input = NewNotification {
        user_id: notification.user_id,
        actor_id: notification.actor_id,
        repo_id: notification.repo_id,
        kind: &notification.kind,
        title: &notification.title,
        body: notification.body.as_deref(),
        link: notification.link.as_deref(),
        metadata: &notification.metadata,
    };
    let thread = match ensure(connection, &input).await {
        Ok(thread) => thread,
        Err(error)
            if error.is::<crate::backend::db::notifications::records::NotificationSuppressed>() =>
        {
            sqlx::query("DELETE FROM notifications WHERE id=?1")
                .bind(id)
                .execute(connection)
                .await?;
            return Ok(false);
        }
        Err(error) => return Err(error),
    };
    sqlx::query("UPDATE notifications SET thread_id=?1 WHERE id=?2")
        .bind(thread)
        .bind(id)
        .execute(connection)
        .await?;
    Ok(true)
}
fn readable(scopes: &NotificationScopes) -> Vec<i64> {
    scopes
        .readable
        .iter()
        .filter_map(|(id, yes)| yes.then_some(*id))
        .collect()
}
async fn candidates(
    connection: &mut sqlx::SqliteConnection,
    user: i64,
    scopes: &NotificationScopes,
    filter: &ThreadFilter,
    thread: Option<i64>,
) -> Result<Vec<ThreadSummary>> {
    let ids = readable(scopes);
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    Ok(sqlx::query_as(r#"
    WITH grouped AS (
        SELECT t.id AS thread_id,t.scope_repository_id,MAX(n.id) AS latest_id,MAX(n.created_at) AS updated_at,MAX(n.read_at) AS last_read_at,
               MAX(n.read_at IS NULL AND n.done_at IS NULL)>0 AS unread,MAX(n.done_at IS NULL)>0 AS inbox
        FROM notification_threads t JOIN notifications n ON n.thread_id=t.id AND n.user_id=t.user_id AND n.scope_repository_id=t.scope_repository_id
        WHERE t.user_id=?1 AND n.id<=?2 AND t.scope_repository_id IN (SELECT value FROM json_each(?3))
          AND (?4 IS NULL OR t.scope_repository_id=?4) AND (?5 IS NULL OR t.id=?5)
          AND (NOT ?6 OR EXISTS(SELECT 1 FROM notifications d WHERE d.thread_id=t.id AND d.id<=?2 AND d.kind IN ('mention','assigned','pull_request_review_requested','issue_assigned','pull_request_assigned','pull_request_comment_reply'))
            OR (t.subject_type='Discussion' AND (
                EXISTS(SELECT 1 FROM pull_requests p WHERE p.repo_id=t.scope_repository_id AND CAST(p.number AS TEXT)=t.subject_key AND p.author_id=?1)
                OR EXISTS(SELECT 1 FROM issues i WHERE i.repo_id=t.scope_repository_id AND CAST(i.number AS TEXT)=t.subject_key AND i.author_id=?1)
                OR EXISTS(SELECT 1 FROM pr_comments c JOIN pull_requests p ON p.id=c.pull_request_id WHERE p.repo_id=t.scope_repository_id AND CAST(p.number AS TEXT)=t.subject_key AND c.author_id=?1 AND NOT EXISTS(SELECT 1 FROM pr_reviews v WHERE v.id=c.review_id AND v.state='pending'))
                OR EXISTS(SELECT 1 FROM issue_comments c JOIN issues i ON i.id=c.issue_id WHERE i.repo_id=t.scope_repository_id AND CAST(i.number AS TEXT)=t.subject_key AND c.author_id=?1))))
        GROUP BY t.id,t.scope_repository_id
    ) SELECT thread_id,scope_repository_id,latest_id,updated_at,last_read_at,unread FROM grouped
      WHERE (?5 IS NOT NULL OR inbox) AND (?7 OR unread)
        AND (?8 IS NULL OR julianday(updated_at)>julianday(?8)) AND (?9 IS NULL OR julianday(updated_at)<julianday(?9))
      ORDER BY updated_at DESC,thread_id DESC LIMIT ?10 OFFSET ?11
    "#).bind(user).bind(scopes.high_water).bind(serde_json::to_string(&ids)?).bind(filter.repository_id).bind(thread).bind(filter.participating).bind(filter.all).bind(filter.since.map(|d|d.to_rfc3339())).bind(filter.before.map(|d|d.to_rfc3339())).bind(filter.limit.clamp(1,101)).bind(filter.offset.max(0)).fetch_all(connection).await?)
}
async fn view(
    connection: &mut sqlx::SqliteConnection,
    user: i64,
    summary: ThreadSummary,
) -> Result<Option<ThreadView>> {
    let thread: Option<NotificationThread> =
        sqlx::query_as("SELECT * FROM notification_threads WHERE id=?1 AND user_id=?2")
            .bind(summary.thread_id)
            .bind(user)
            .fetch_optional(&mut *connection)
            .await?;
    let notification: Option<Notification> = sqlx::query_as(
        "SELECT * FROM notifications WHERE id=?1 AND user_id=?2 AND scope_repository_id=?3",
    )
    .bind(summary.latest_id)
    .bind(user)
    .bind(summary.scope_repository_id)
    .fetch_optional(&mut *connection)
    .await?;
    let repo: Option<Repository> = sqlx::query_as("SELECT * FROM repositories WHERE id=?1")
        .bind(summary.scope_repository_id)
        .fetch_optional(&mut *connection)
        .await?;
    let (Some(thread), Some(notification), Some(repo)) = (thread, notification, repo) else {
        return Ok(None);
    };
    let owner:Option<String>=sqlx::query_scalar("SELECT CASE WHEN r.owner_kind='user' THEN u.username ELSE o.name END FROM repositories r LEFT JOIN users u ON r.owner_user_id=u.id LEFT JOIN organizations o ON r.owner_org_id=o.id WHERE r.id=?1").bind(repo.id).fetch_optional(&mut *connection).await?.flatten();
    let Some(owner) = owner else {
        return Ok(None);
    };
    let target = if thread.subject_type == "Discussion" {
        let discussion:Option<(bool,i32,String,i64)>=sqlx::query_as("SELECT TRUE AS is_pull,number,title,author_id FROM pull_requests WHERE repo_id=?1 AND CAST(number AS TEXT)=?2 UNION ALL SELECT FALSE AS is_pull,number,title,author_id FROM issues WHERE repo_id=?1 AND CAST(number AS TEXT)=?2 ORDER BY is_pull DESC LIMIT 1").bind(repo.id).bind(&thread.subject_key).fetch_optional(&mut *connection).await?;
        discussion.map_or(ThreadTarget::Other, |(pull, number, title, author)| {
            if pull {
                ThreadTarget::Pull {
                    number,
                    title,
                    author,
                }
            } else {
                ThreadTarget::Issue {
                    number,
                    title,
                    author,
                }
            }
        })
    } else if thread.subject_type == "Release" {
        let release:Option<(i64,String,i64)>=sqlx::query_as("SELECT id,COALESCE(NULLIF(name,''),tag_name),author_id FROM releases WHERE repo_id=?1 AND tag_name=?2 AND is_draft=0").bind(repo.id).bind(&thread.subject_key).fetch_optional(&mut *connection).await?;
        release.map_or(ThreadTarget::Other, |(id, title, author)| {
            ThreadTarget::Release { id, title, author }
        })
    } else {
        ThreadTarget::Other
    };
    Ok(Some(ThreadView::new(
        thread,
        notification,
        repo,
        owner,
        summary,
        target,
    )))
}
async fn subscription(
    connection: &mut sqlx::SqliteConnection,
    id: i64,
) -> Result<Option<ThreadSubscription>> {
    Ok(
        sqlx::query_as("SELECT * FROM notification_thread_subscriptions WHERE thread_id=?1")
            .bind(id)
            .fetch_optional(connection)
            .await?,
    )
}
async fn manage_subscription(
    connection: &mut sqlx::SqliteConnection,
    user: i64,
    id: i64,
    changes: Option<(bool, bool)>,
) -> Result<ThreadInbox> {
    let thread: Option<NotificationThread> =
        sqlx::query_as("SELECT * FROM notification_threads WHERE id=?1 AND user_id=?2")
            .bind(id)
            .bind(user)
            .fetch_optional(&mut *connection)
            .await?;
    let Some(thread) = thread else {
        return Ok(ThreadInbox::default());
    };
    let condition = super::scope_readable_condition();
    let allowed:bool=sqlx::query_scalar(&format!("SELECT EXISTS(SELECT 1 FROM repositories r JOIN users u ON u.id=?1 WHERE r.id=?2 AND {condition})")).bind(user).bind(thread.scope_repository_id).fetch_one(&mut *connection).await?;
    if !allowed {
        return Ok(ThreadInbox::default());
    }
    let updated = if let Some((subscribed, ignored)) = changes {
        sqlx::query("INSERT INTO notification_thread_subscriptions(thread_id,subscribed,ignored,is_explicit) VALUES(?1,?2,?3,TRUE) ON CONFLICT(thread_id) DO UPDATE SET subscribed=excluded.subscribed,ignored=excluded.ignored,is_explicit=TRUE").bind(id).bind(subscribed).bind(ignored).execute(&mut *connection).await?.rows_affected()
    } else {
        0
    };
    let subscription = subscription(connection, id).await?;
    Ok(ThreadInbox {
        thread: Some(thread),
        subscription,
        found: true,
        updated,
        ..Default::default()
    })
}
pub(super) async fn perform(
    connection: &mut sqlx::SqliteConnection,
    user: i64,
    scopes: &NotificationScopes,
    action: ThreadAction,
) -> Result<ThreadInbox> {
    let repository = match &action {
        ThreadAction::List(f) => f.repository_id,
        ThreadAction::ReadAll { repository_id, .. } => *repository_id,
        _ => None,
    };
    if let Some(repo) = repository {
        let condition = super::scope_readable_condition();
        let allowed:bool=sqlx::query_scalar(&format!("SELECT EXISTS(SELECT 1 FROM repositories r JOIN users u ON u.id=?1 WHERE r.id=?2 AND {condition})")).bind(user).bind(repo).fetch_one(&mut *connection).await?;
        if !allowed {
            return Ok(ThreadInbox::default());
        }
    }
    match action {
        ThreadAction::List(filter) => {
            let candidates = candidates(connection, user, scopes, &filter, None).await?;
            let mut result = ThreadInbox {
                found: true,
                ..Default::default()
            };
            for candidate in candidates {
                if let Some(view) = view(connection, user, candidate).await? {
                    result.threads.push(view);
                }
            }
            Ok(result)
        }
        ThreadAction::ReadAll {
            repository_id,
            last_read_at,
            read,
        } => {
            let ids = readable(scopes);
            let updated=sqlx::query(r#"UPDATE notifications AS n SET read_at=CASE WHEN ?1 THEN strftime('%Y-%m-%dT%H:%M:%f000Z','now') ELSE NULL END
                WHERE n.user_id=?2 AND n.id<=?3 AND n.thread_id IS NOT NULL AND n.scope_repository_id IN (SELECT value FROM json_each(?4))
                  AND (?5 IS NULL OR n.scope_repository_id=?5) AND n.done_at IS NULL AND ((?1 AND n.read_at IS NULL) OR (NOT ?1 AND n.read_at IS NOT NULL))
                  AND (?6 IS NULL OR n.thread_id IN (SELECT thread_id FROM notifications WHERE user_id=?2 AND id<=?3 GROUP BY thread_id HAVING julianday(MAX(created_at))<=julianday(?6)))"#)
                .bind(read).bind(user).bind(scopes.high_water).bind(serde_json::to_string(&ids)?).bind(repository_id).bind(last_read_at.map(|d|d.to_rfc3339())).execute(connection).await?.rows_affected();
            Ok(ThreadInbox {
                updated,
                found: true,
                ..Default::default()
            })
        }
        ThreadAction::GetSubscription(id) => manage_subscription(connection, user, id, None).await,
        ThreadAction::SetSubscription {
            thread_id,
            subscribed,
            ignored,
        } => manage_subscription(connection, user, thread_id, Some((subscribed, ignored))).await,
        ThreadAction::DeleteSubscription(id) => {
            manage_subscription(connection, user, id, Some((false, false))).await
        }
        action @ (ThreadAction::Get(_) | ThreadAction::Read(_) | ThreadAction::Done(_)) => {
            let id = match action {
                ThreadAction::Get(id) | ThreadAction::Read(id) | ThreadAction::Done(id) => id,
                _ => unreachable!("single notification action"),
            };
            let filter = ThreadFilter {
                all: true,
                limit: 1,
                ..Default::default()
            };
            let Some(candidate) = candidates(connection, user, scopes, &filter, Some(id))
                .await?
                .pop()
            else {
                return Ok(ThreadInbox::default());
            };
            let Some(view) = view(connection, user, candidate).await? else {
                return Ok(ThreadInbox::default());
            };
            let mut result = ThreadInbox {
                found: true,
                ..Default::default()
            };
            match action {
                ThreadAction::Read(_) => {
                    result.updated=sqlx::query("UPDATE notifications SET read_at=strftime('%Y-%m-%dT%H:%M:%f000Z','now') WHERE user_id=?1 AND thread_id=?2 AND id<=?3 AND read_at IS NULL").bind(user).bind(id).bind(scopes.high_water).execute(&mut *connection).await?.rows_affected();
                }
                ThreadAction::Done(_) => {
                    result.updated=sqlx::query("UPDATE notifications SET done_at=COALESCE(done_at,strftime('%Y-%m-%dT%H:%M:%f000Z','now')),read_at=COALESCE(read_at,strftime('%Y-%m-%dT%H:%M:%f000Z','now')) WHERE user_id=?1 AND thread_id=?2 AND id<=?3 AND (done_at IS NULL OR read_at IS NULL)").bind(user).bind(id).bind(scopes.high_water).execute(&mut *connection).await?.rows_affected();
                }
                ThreadAction::Get(_) => {}
                _ => unreachable!("list and bulk actions handled above"),
            }
            result.subscription = subscription(connection, id).await?;
            result.threads.push(view);
            Ok(result)
        }
    }
}
