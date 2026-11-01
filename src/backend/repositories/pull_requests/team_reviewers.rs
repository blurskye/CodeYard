//! Shared manual team requests and notification delivery for native and GitHub adapters.
use super::OperationError;
use crate::backend::{db, notifications::NotificationBroadcaster};
use db::models::{PullRequest, Repository, Team, User};

pub async fn set_requests(
    pool: &db::DbPool,
    broadcaster: &NotificationBroadcaster,
    actor: &User,
    repo: &Repository,
    pull: &PullRequest,
    input: &db::pull_requests::team_reviewers::ReviewRequests<'_>,
) -> Result<(), OperationError> {
    let owner = db::repositories::records::repository_owner_name(pool, repo.id)
        .await
        .map_err(super::database_error)?
        .ok_or_else(|| OperationError::Missing {
            code: "not_found",
            message: "Repository not found".into(),
        })?;
    let changed = db::pull_requests::team_reviewers::set_requests(pool, actor.id, pull.id, input)
        .await
        .map_err(error)?;
    for user in &changed.added_users {
        super::reviews::notify_review_request(pool, broadcaster, actor, &owner, repo, pull, user)
            .await;
    }
    notify(pool, broadcaster, actor, repo, pull, &changed.added_teams).await;
    Ok(())
}

pub async fn change(
    pool: &db::DbPool,
    broadcaster: &NotificationBroadcaster,
    actor: &User,
    repo: &Repository,
    pull: &PullRequest,
    slugs: &[String],
    action: db::pull_requests::team_reviewers::ReviewTeamAction,
) -> Result<Vec<Team>, OperationError> {
    let changed =
        db::pull_requests::team_reviewers::change(pool, actor.id, pull.id, slugs, action, None)
            .await
            .map_err(error)?;
    if action == db::pull_requests::team_reviewers::ReviewTeamAction::Remove {
        emit_events(
            pool,
            None,
            repo,
            pull,
            "review_request_removed",
            actor.id,
            &changed,
        )
        .await;
    } else {
        notify(pool, broadcaster, actor, repo, pull, &changed).await;
        emit_events(
            pool,
            None,
            repo,
            pull,
            "review_requested",
            actor.id,
            &changed,
        )
        .await;
    }
    Ok(changed)
}

/// Sends one `pull_request` event for each team whose review request was added or removed.
pub(super) async fn emit_events(
    pool: &db::DbPool,
    cluster: Option<&crate::backend::cluster::NodeContext>,
    repo: &Repository,
    pull: &PullRequest,
    action: &'static str,
    sender_id: i64,
    teams: &[Team],
) {
    if teams.is_empty() {
        return;
    }
    let Ok(Some(org)) = db::ci::ci_repository_owner(pool, repo.id).await else {
        tracing::warn!(
            repo_id = repo.id,
            "review team events skipped: owner unavailable"
        );
        return;
    };
    // A team that lost its access since is sent without a permission.
    let access = db::pull_requests::team_reviewers::list(pool, repo.id, None, None)
        .await
        .unwrap_or_default();
    for team in teams {
        let permission = access
            .iter()
            .find(|row| row.team.id == team.id)
            .map_or(0, |row| row.permission);
        let mut extra = serde_json::Map::new();
        extra.insert(
            "requested_team".into(),
            crate::backend::webhooks::payloads::team_document(team, &org, permission),
        );
        super::emit_pull_request_event(pool, cluster, repo, pull, action, sender_id, extra).await;
    }
}

pub(super) fn error(error: anyhow::Error) -> OperationError {
    if error.is::<db::pull_requests::team_reviewers::InvalidReviewTeam>() {
        OperationError::Invalid {
            code: "invalid_review_team",
            message: error.to_string(),
        }
    } else {
        super::write_error(error)
    }
}

pub(super) async fn notify(
    pool: &db::DbPool,
    broadcaster: &NotificationBroadcaster,
    actor: &User,
    repo: &Repository,
    pull: &PullRequest,
    teams: &[Team],
) {
    let ids: Vec<_> = teams.iter().map(|team| team.id).collect();
    if ids.is_empty() {
        return;
    }
    let Ok(Some(owner)) = db::ci::ci_repository_owner(pool, repo.id).await else {
        return;
    };
    let mut after = 0;
    loop {
        let members =
            match db::pull_requests::team_reviewers::members(pool, repo.id, &ids, after).await {
                Ok(members) => members,
                Err(error) => {
                    tracing::warn!(%error, "Review team notification recipients unavailable");
                    return;
                }
            };
        let Some(last) = members.last() else { return };
        after = last.id;
        let short = members.len() < 100;
        for user in members {
            if user.id != pull.author_id {
                super::reviews::notify_review_request(
                    pool,
                    broadcaster,
                    actor,
                    &owner,
                    repo,
                    pull,
                    &user,
                )
                .await;
            }
        }
        if short {
            return;
        }
    }
}
