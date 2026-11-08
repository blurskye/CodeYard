use crate::models::Notification;
use crate::queries::notifications::records::NewNotification;
use anyhow::Result;
pub async fn ensure(
    connection: &mut sqlx::PgConnection,
    input: &NewNotification<'_>,
) -> Result<Option<i64>> {
    let Some((kind, key)) = crate::queries::notifications::threads::subject(input) else {
        return Ok(None);
    };
    let id:i64=sqlx::query_scalar("INSERT INTO notification_threads(user_id,scope_repository_id,subject_type,subject_key) VALUES($1,$2,$3,$4) ON CONFLICT(user_id,scope_repository_id,subject_type,subject_key) DO UPDATE SET subject_key=EXCLUDED.subject_key RETURNING id").bind(input.user_id).bind(input.repo_id).bind(kind).bind(key).fetch_one(&mut *connection).await?;
    sqlx::query("INSERT INTO notification_thread_subscriptions(thread_id) VALUES($1) ON CONFLICT(thread_id) DO NOTHING").bind(id).execute(&mut *connection).await?;
    let (subscribed,ignored):(bool,bool)=sqlx::query_as("SELECT subscribed,ignored FROM notification_thread_subscriptions WHERE thread_id=$1 FOR SHARE").bind(id).fetch_one(connection).await?;
    anyhow::ensure!(
        crate::queries::notifications::threads::allows_delivery(subscribed, ignored, input.kind),
        crate::queries::notifications::records::NotificationSuppressed
    );
    Ok(Some(id))
}

pub(crate) async fn attach_existing(connection: &mut sqlx::PgConnection, id: i64) -> Result<bool> {
    let notification: Notification = sqlx::query_as("SELECT * FROM notifications WHERE id=$1")
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
            if error.is::<crate::queries::notifications::records::NotificationSuppressed>() =>
        {
            sqlx::query("DELETE FROM notifications WHERE id=$1")
                .bind(id)
                .execute(connection)
                .await?;
            return Ok(false);
        }
        Err(error) => return Err(error),
    };
    sqlx::query("UPDATE notifications SET thread_id=$1 WHERE id=$2")
        .bind(thread)
        .bind(id)
        .execute(connection)
        .await?;
    Ok(true)
}
