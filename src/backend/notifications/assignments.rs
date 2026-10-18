//! Assignment notifications. A user who is newly assigned to an issue or pull request hears about
//! it, unless they assigned themselves, are restricted, cannot read the repository, or ignore it.
use super::create::{create_and_broadcast_notification, NotificationRequest};
use super::NotificationBroadcaster;
use crate::backend::db;
use crate::backend::repositories::access::can_read;
use serde_json::json;

/// The issue or pull request that users were assigned to.
pub struct Assignment<'a> {
    pub actor: &'a db::models::User,
    pub repository: &'a db::models::Repository,
    pub owner: &'a str,
    pub repo: &'a str,
    pub number: i32,
    pub pull_request: bool,
    pub title: &'a str,
    pub link: &'a str,
}

/// Notifies each user in `assignees` that the actor assigned them. The assignment has already
/// been saved, so a failure here never fails the request: a failed lookup is logged and the
/// notification is dropped.
pub async fn notify_assignees(
    pool: &db::DbPool,
    broadcaster: &NotificationBroadcaster,
    assignment: &Assignment<'_>,
    assignees: &[i64],
) {
    let noun = if assignment.pull_request {
        "pull request"
    } else {
        "issue"
    };
    let title = format!(
        "{} assigned you to {noun} #{} in {}/{}",
        assignment.actor.username, assignment.number, assignment.owner, assignment.repo
    );
    for &assignee in assignees {
        if assignee == assignment.actor.id {
            continue;
        }
        let user = match db::users::accounts::get_user_by_id(pool, assignee).await {
            Ok(Some(user)) => user,
            Ok(None) => continue,
            Err(error) => {
                tracing::warn!(%error, "Assignment notification skipped: user lookup failed");
                continue;
            }
        };
        // Restricted accounts get nothing, and neither does a user who cannot read the repository.
        if user.is_banned || user.must_change_password {
            continue;
        }
        match can_read(pool, Some(&user), assignment.repository).await {
            Ok(true) => {}
            Ok(false) => continue,
            Err(error) => {
                tracing::warn!(%error, "Assignment notification skipped: repository access check failed");
                continue;
            }
        }
        // Ignoring the repository drops the notification inside the shared create path.
        let _ = create_and_broadcast_notification(
            pool,
            broadcaster,
            NotificationRequest {
                user_id: user.id,
                actor_id: Some(assignment.actor.id),
                repo_id: Some(assignment.repository.id),
                kind: "assigned",
                title: &title,
                body: Some(assignment.title),
                link: Some(assignment.link),
                metadata: json!({
                    "repo_owner": assignment.owner,
                    "repo_name": assignment.repo,
                    "number": assignment.number,
                    "subject": assignment.title,
                }),
            },
        )
        .await;
    }
}
