use super::*;
use crate::backend::db::{
    models::Repository,
    notifications::threads::{
        self as model, NotificationThread, ThreadAction, ThreadFilter, ThreadInbox,
        ThreadSubscription, ThreadSummary, ThreadView,
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
    Ok(sqlx::query_scalar(&format!("SELECT t.user_id FROM notification_threads t JOIN notification_thread_subscriptions s ON s.thread_id=t.id JOIN users u ON u.id=t.user_id JOIN repositories r ON r.id=t.scope_repository_id WHERE t.scope_repository_id=?1 AND t.subject_type=?2 AND t.subject_key=?3 AND s.subscribed AND NOT s.ignored AND NOT u.is_banned AND NOT u.must_change_password AND {scope} AND NOT EXISTS(SELECT 1 FROM watches w WHERE w.repo_id=?1 AND w.user_id=t.user_id AND w.level='ignoring') ORDER BY t.user_id")).bind(repo_id).bind(kind).bind(key).fetch_all(pool).await?)
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
    let sql = r#"
    WITH page AS MATERIALIZED (
        SELECT t.id,t.scope_repository_id,
          (SELECT n.created_at FROM notifications n WHERE n.thread_id=t.id AND n.user_id=?1 AND n.scope_repository_id=t.scope_repository_id AND n.id<=?2 ORDER BY n.created_at DESC LIMIT 1) AS updated_at
        FROM notification_threads t
        WHERE t.user_id=?1 AND t.scope_repository_id IN (SELECT value FROM json_each(?3))
          AND (?4 IS NULL OR t.scope_repository_id=?4) AND (?5 IS NULL OR t.id=?5)
          AND EXISTS(SELECT 1 FROM notifications n WHERE n.thread_id=t.id AND n.user_id=?1 AND n.scope_repository_id=t.scope_repository_id AND n.id<=?2)
          AND (?5 IS NOT NULL OR EXISTS(SELECT 1 FROM notifications n WHERE n.thread_id=t.id AND n.user_id=?1 AND n.scope_repository_id=t.scope_repository_id AND n.id<=?2 AND n.done_at IS NULL))
          AND (?7 OR EXISTS(SELECT 1 FROM notifications n WHERE n.thread_id=t.id AND n.user_id=?1 AND n.scope_repository_id=t.scope_repository_id AND n.id<=?2 AND n.read_at IS NULL AND n.done_at IS NULL))
          AND (?8 IS NULL OR EXISTS(SELECT 1 FROM notifications n WHERE n.thread_id=t.id AND n.user_id=?1 AND n.scope_repository_id=t.scope_repository_id AND n.id<=?2 AND n.created_at>?8))
          AND (?9 IS NULL OR NOT EXISTS(SELECT 1 FROM notifications n WHERE n.thread_id=t.id AND n.user_id=?1 AND n.scope_repository_id=t.scope_repository_id AND n.id<=?2 AND n.created_at>=?9))
          AND (NOT ?6 OR EXISTS(SELECT 1 FROM notifications d WHERE d.thread_id=t.id AND d.id<=?2 AND d.kind IN ('mention','assigned','pull_request_review_requested','issue_assigned','pull_request_assigned','pull_request_comment_reply'))
            OR (t.subject_type='Discussion' AND (
                EXISTS(SELECT 1 FROM pull_requests p WHERE p.repo_id=t.scope_repository_id AND CAST(p.number AS TEXT)=t.subject_key AND p.author_id=?1)
                OR EXISTS(SELECT 1 FROM issues i WHERE i.repo_id=t.scope_repository_id AND CAST(i.number AS TEXT)=t.subject_key AND i.author_id=?1)
                OR EXISTS(SELECT 1 FROM pr_reviews v JOIN pull_requests p ON p.id=v.pull_request_id WHERE p.repo_id=t.scope_repository_id AND CAST(p.number AS TEXT)=t.subject_key AND v.author_id=?1 AND v.state<>'pending')
                OR EXISTS(SELECT 1 FROM pr_comments c JOIN pull_requests p ON p.id=c.pull_request_id WHERE p.repo_id=t.scope_repository_id AND CAST(p.number AS TEXT)=t.subject_key AND c.author_id=?1 AND NOT EXISTS(SELECT 1 FROM pr_reviews v WHERE v.id=c.review_id AND v.state='pending'))
                OR EXISTS(SELECT 1 FROM issue_comments c JOIN issues i ON i.id=c.issue_id WHERE i.repo_id=t.scope_repository_id AND CAST(i.number AS TEXT)=t.subject_key AND c.author_id=?1))) OR (t.subject_type='Commit' AND EXISTS(SELECT 1 FROM commit_comments c WHERE c.repo_id=t.scope_repository_id AND c.commit_sha=t.subject_key AND c.author_id=?1)))
        ORDER BY updated_at DESC,t.id DESC LIMIT ?10 OFFSET ?11
    ) SELECT t.id AS thread_id,t.scope_repository_id,MAX(n.id) AS latest_id,t.updated_at,MAX(n.read_at) AS last_read_at,
             MAX(n.read_at IS NULL AND n.done_at IS NULL)>0 AS unread
      FROM page t JOIN notifications n ON n.thread_id=t.id AND n.user_id=?1 AND n.scope_repository_id=t.scope_repository_id AND n.id<=?2
      GROUP BY t.id,t.scope_repository_id,t.updated_at ORDER BY t.updated_at DESC,t.id DESC
    "#;
    let sql = if filter.since.is_some() {
        sql.replacen("WITH page", "WITH recent AS MATERIALIZED (SELECT DISTINCT thread_id FROM notifications WHERE user_id=?1 AND created_at>?8 AND id<=?2 AND thread_id IS NOT NULL), page", 1)
            .replacen("FROM notification_threads t\n", "FROM notification_threads t JOIN recent ON recent.thread_id=t.id\n", 1)
    } else { sql.to_owned() };
    Ok(sqlx::query_as(&sql).bind(user).bind(scopes.high_water).bind(serde_json::to_string(&ids)?).bind(filter.repository_id).bind(thread).bind(filter.participating).bind(filter.all).bind(filter.since.map(|d|d.to_rfc3339_opts(chrono::SecondsFormat::Micros,true))).bind(filter.before.map(|d|d.to_rfc3339_opts(chrono::SecondsFormat::Micros,true))).bind(filter.limit.clamp(1,101)).bind(filter.offset.max(0)).fetch_all(connection).await?)
}
async fn views(
    connection: &mut sqlx::SqliteConnection,
    user: i64,
    summaries: Vec<ThreadSummary>,
) -> Result<Vec<ThreadView>> {
    if summaries.is_empty() { return Ok(Vec::new()); }
    let ids = serde_json::to_string(&summaries.iter().map(|s| s.thread_id).collect::<Vec<_>>())?;
    let latest = serde_json::to_string(&summaries.iter().map(|s| s.latest_id).collect::<Vec<_>>())?;
    let repos = serde_json::to_string(&summaries.iter().map(|s| s.scope_repository_id).collect::<Vec<_>>())?;
    let threads = sqlx::query_as("SELECT * FROM notification_threads WHERE id IN (SELECT value FROM json_each(?1)) AND user_id=?2")
        .bind(&ids).bind(user).fetch_all(&mut *connection).await?;
    let mut notifications: Vec<Notification> = sqlx::query_as("SELECT n.* FROM notifications n JOIN notification_threads t ON t.id=n.thread_id AND t.scope_repository_id=n.scope_repository_id WHERE n.id IN (SELECT value FROM json_each(?1)) AND n.user_id=?2 AND t.user_id=?2 AND t.id IN (SELECT value FROM json_each(?3))")
        .bind(&latest).bind(user).bind(&ids).fetch_all(&mut *connection).await?;
    super::refresh_links(connection, &mut notifications).await?;
    let repositories: Vec<Repository> = sqlx::query_as("SELECT * FROM repositories WHERE id IN (SELECT value FROM json_each(?1))")
        .bind(&repos).fetch_all(&mut *connection).await?;
    let projections: Vec<model::ThreadProjection> = sqlx::query_as(r#"
        SELECT t.id AS thread_id,CASE WHEN r.owner_kind='user' THEN u.username ELSE o.name END AS owner,
          p.number AS pull_number,i.number AS issue_number,e.id AS release_id,
          COALESCE(p.title,i.title,NULLIF(e.name,''),e.tag_name) AS title,COALESCE(p.author_id,i.author_id,e.author_id) AS author,
          c.id AS comment_id,(c.path IS NOT NULL) AS inline
        FROM notification_threads t JOIN repositories r ON r.id=t.scope_repository_id
        LEFT JOIN users u ON u.id=r.owner_user_id LEFT JOIN organizations o ON o.id=r.owner_org_id
        LEFT JOIN pull_requests p ON t.subject_type='Discussion' AND p.repo_id=r.id AND CAST(p.number AS TEXT)=t.subject_key
        LEFT JOIN issues i ON t.subject_type='Discussion' AND p.id IS NULL AND i.repo_id=r.id AND CAST(i.number AS TEXT)=t.subject_key
        LEFT JOIN releases e ON t.subject_type='Release' AND e.repo_id=r.id AND e.tag_name=t.subject_key AND NOT e.is_draft
        JOIN notifications n ON n.thread_id=t.id AND n.id IN (SELECT value FROM json_each(?3)) AND n.user_id=?2 AND n.scope_repository_id=r.id
        LEFT JOIN pr_comments c ON c.pull_request_id=p.id AND CAST(c.id AS TEXT)=CAST(COALESCE(json_extract(n.metadata,'$.pull_request_comment_id'),json_extract(n.metadata,'$.comment_id')) AS TEXT)
          AND (c.review_id IS NULL OR EXISTS(SELECT 1 FROM pr_reviews v WHERE v.id=c.review_id AND v.state<>'pending'))
        WHERE t.id IN (SELECT value FROM json_each(?1)) AND t.user_id=?2
    "#).bind(&ids).bind(user).bind(&latest).fetch_all(connection).await?;
    Ok(model::hydrate(summaries, threads, notifications, repositories, projections))
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
            Ok(ThreadInbox {
                found: true,
                threads: views(connection, user, candidates).await?,
                ..Default::default()
            })
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
            let Some(view) = views(connection, user, vec![candidate]).await?.pop() else {
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
