use crate::context::NodeContext;
use codeyard_notifications::broadcast::NotificationBroadcaster;
use codeyard_notifications::create::{create_and_broadcast_notification, NotificationRequest};
use serde_json::json;
/// Sends the `pull_request` event for a review request to `target`, or its removal.
pub async fn emit_reviewer_event(
    pool: &codeyard_db_core::DbPool,
    cluster: Option<&NodeContext>,
    repository: &codeyard_db_core::models::Repository,
    pull_request: &codeyard_db_core::models::PullRequest,
    action: &'static str,
    sender_id: i64,
    target: &codeyard_db_core::models::User,
) {
    let mut extra = serde_json::Map::new();
    extra.insert(
        "requested_reviewer".into(),
        codeyard_webhooks::payloads::user(target),
    );
    crate::repositories::pull_requests::emit_pull_request_event(
        pool,
        cluster,
        repository,
        pull_request,
        action,
        sender_id,
        extra,
    )
    .await;
}

pub async fn notify_review_request(
    pool: &codeyard_db_core::DbPool,
    broadcaster: &NotificationBroadcaster,
    requester: &codeyard_db_core::models::User,
    owner: &str,
    repository: &codeyard_db_core::models::Repository,
    pull_request: &codeyard_db_core::models::PullRequest,
    target: &codeyard_db_core::models::User,
) {
    if target.id != requester.id {
        let title = format!(
            "{} requested your review on pull request #{}",
            requester.username, pull_request.number
        );
        let link = format!(
            "/{}/{}/pulls/{}",
            owner, repository.name, pull_request.number
        );
        let _ = create_and_broadcast_notification(
            pool,
            broadcaster,
            NotificationRequest {
                user_id: target.id,
                actor_id: Some(requester.id),
                repo_id: Some(repository.id),
                kind: "pull_request_review_requested",
                title: &title,
                body: Some(&pull_request.title),
                link: Some(&link),
                metadata: json!({
                    "repo_owner": owner,
                    "repo_name": repository.name,
                    "pull_number": pull_request.number,
                    "subject": pull_request.title,
                }),
            },
        )
        .await;
    }
}
