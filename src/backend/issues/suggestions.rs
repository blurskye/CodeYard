//! Shared post-commit effects for REST and native suggestion review.
use crate::backend::{
    db::{
        self,
        models::{Repository, User},
    },
    notifications::{
        assignments::{self, Assignment},
        NotificationBroadcaster,
    },
    webhooks::{
        self, events,
        payloads::{self, Scope},
    },
};
use anyhow::Result;
pub async fn resolve(
    pool: &db::DbPool,
    broadcaster: &NotificationBroadcaster,
    actor: &User,
    credential: &db::users::accounts::AccountCredential<'_>,
    (repo, owner): (&Repository, &str),
    (issue_id, suggestion_id): (i64, i64),
    approve: bool,
) -> Result<db::suggestions::Suggestion> {
    let outcome = db::suggestions::resolve(
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
        return Err(db::suggestions::Refusal::Invalidated.into());
    }
    if let Some(assignee) = outcome.assignment {
        let issue = &outcome.issue;
        let link = format!(
            "/{}/{}/issues/{}",
            codeyard_webui::encode_component(owner),
            codeyard_webui::encode_component(&repo.name),
            issue.number
        );
        assignments::notify_assignees(
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
        super::mutations::notify_state_change(pool, broadcaster, actor, (repo, owner), &issue)
            .await;
        let (author, sender) = (issue.author_id, actor.id);
        let repository = repo.clone();
        webhooks::emit(
            pool,
            repo,
            events::ISSUES,
            vec![author, sender],
            move |owner, people| {
                payloads::issues(
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
