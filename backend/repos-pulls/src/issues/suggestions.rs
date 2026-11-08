//! Shared post-commit effects for REST and native suggestion review.

use anyhow::Result;
use codeyard_db_core::models::Repository;
use codeyard_db_core::models::User;
use codeyard_notifications::assignments::Assignment;
use codeyard_notifications::broadcast::NotificationBroadcaster;
use codeyard_webhooks::payloads::Scope;

pub async fn resolve(
    pool: &codeyard_db_core::DbPool,
    broadcaster: &NotificationBroadcaster,
    actor: &User,
    credential: &codeyard_db_core::queries::users::accounts::AccountCredential<'_>,
    (repo, owner): (&Repository, &str),
    (issue_id, suggestion_id): (i64, i64),
    approve: bool,
) -> Result<codeyard_db_repos::suggestions::Suggestion> {
    let outcome = codeyard_db_repos::suggestions::resolve(
        pool,
        repo.id,
        actor.id,
        credential,
        issue_id,
        suggestion_id,
        approve,
    )
    .await?;
    if outcome.invalidated {
        return Err(codeyard_db_repos::suggestions::Refusal::Invalidated.into());
    }
    if let Some(assignee) = outcome.assignment {
        let issue = &outcome.issue;
        let link = format!(
            "/{}/{}/issues/{}",
            codeyard_types::paths::encode_component(owner),
            codeyard_types::paths::encode_component(&repo.name),
            issue.number
        );
        codeyard_notifications::assignments::notify_assignees(
            pool,
            broadcaster,
            &Assignment {
                actor,
                repository: repo,
                owner,
                repo: &repo.name,
                number: issue.number,
                pull_request: false,
                title: &issue.title,
                link: &link,
            },
            &[assignee],
        )
        .await;
    }
    if let Some(issue) = outcome.closed_issue {
        crate::issues::mutations::notify_state_change(
            pool,
            broadcaster,
            actor,
            (repo, owner),
            &issue,
        )
        .await;
        let (author, sender) = (issue.author_id, actor.id);
        let repository = repo.clone();
        codeyard_repos_core::webhooks::emit(
            pool,
            repo,
            codeyard_model::webhook_events::ISSUES,
            vec![author, sender],
            move |owner, people| {
                codeyard_webhooks::payloads::issues(
                    "closed",
                    &issue,
                    &people.get(author),
                    Scope {
                        repo: &repository,
                        owner,
                        sender: &people.get(sender),
                    },
                )
            },
        )
        .await;
    }
    Ok(outcome.suggestion)
}
