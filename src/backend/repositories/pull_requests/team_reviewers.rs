//! Shared manual team requests and notification delivery for native and GitHub adapters.
use super::OperationError;
use crate::backend::{db, notifications::NotificationBroadcaster};
use db::models::{PullRequest, Repository, Team, User};

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
        notify(pool, broadcaster, actor, repo, pull, &changed).await;
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
    teams: &[Team],
) {
    let ids: Vec<_> = teams.iter().map(|team| team.id).collect();
    if ids.is_empty() { return; }
    let Ok(Some(owner)) = db::ci::ci_repository_owner(pool, repo.id).await else {
        return;
    };
    let mut after = 0;
    loop {
        let members = match db::pull_requests::team_reviewers::members(pool, repo.id, &ids, after).await {
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
                super::reviews::notify_review_request(pool, broadcaster, actor, &owner, repo, pull, &user).await;
            }
        }
        if short { return; }
    }
}
