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
    let owner = db::repositories::records::repository_owner_name(pool, repo.id).await.map_err(super::database_error)?
        .ok_or_else(|| OperationError::Missing { code: "not_found", message: "Repository not found".into() })?;
    let changed = db::pull_requests::team_reviewers::set_requests(pool, actor.id, pull.id, input).await.map_err(error)?;
    for user in &changed.added_users { super::reviews::notify_review_request(pool, broadcaster, actor, &owner, repo, pull, user).await; }
    for team in &changed.added_teams { notify(pool, broadcaster, actor, repo, pull, team).await; }
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
    if action != db::pull_requests::team_reviewers::ReviewTeamAction::Remove {
        for team in &changed {
            notify(pool, broadcaster, actor, repo, pull, team).await;
        }
    }
    Ok(changed)
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
    team: &Team,
) {
    if !team.notifications_enabled {
        return;
    }
    let Ok(Some(owner)) = db::ci::ci_repository_owner(pool, repo.id).await else {
        return;
    };
    let members = match db::pull_requests::team_reviewers::members(pool, repo.id, team.id).await {
        Ok(members) => members,
        Err(error) => {
            tracing::warn!(%error, "Review team notification recipients unavailable");
            return;
        }
    };
    let mut sent = std::collections::HashSet::new();
    for id in members {
        if id == pull.author_id || !sent.insert(id) {
            continue;
        }
        let Ok(Some(user)) = db::users::accounts::get_user_by_id(pool, id).await else {
            continue;
        };
        if !matches!(
            crate::backend::repositories::access::can_write(pool, &user, repo).await,
            Ok(true)
        ) {
            continue;
        }
        super::reviews::notify_review_request(pool, broadcaster, actor, &owner, repo, pull, &user)
            .await;
    }
}
