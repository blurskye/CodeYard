//! Assigning people to issues and pull requests. The REST and GraphQL APIs, the issue and pull
//! request pages and issue creation all change assignees through these functions, so every change
//! records the same timeline events and notifies the same people.

use crate::backend::db::assignees::AssignedItem;
use crate::backend::notifications::assignments::notify_assignees;
use crate::backend::notifications::assignments::Assignment;
use crate::backend::notifications::NotificationBroadcaster;

/// The issue or pull request whose assignees change, and who changes them.
pub struct Target<'a> {
    pub actor: &'a codeyard_db_core::models::User,
    pub repository: &'a codeyard_db_core::models::Repository,
    pub owner: &'a str,
    pub item: AssignedItem,
    pub number: i32,
    pub title: &'a str,
}

/// Assigns `users` in one transaction. Each user who was not assigned before gets an `assigned`
/// timeline event and a notification. Returns the ids of those users.
pub async fn assign(
    pool: &codeyard_db_core::DbPool,
    broadcaster: &NotificationBroadcaster,
    target: &Target<'_>,
    authority: impl Into<crate::backend::db::credentials::Actor>,
    users: &[codeyard_db_core::models::User],
) -> anyhow::Result<Vec<i64>> {
    let ids: Vec<i64> = users.iter().map(|user| user.id).collect();
    let added = crate::backend::db::assignees::assign_users(
        pool,
        authority,
        target.repository.id,
        target.item,
        &ids,
    )
    .await?;
    let added_users: Vec<&codeyard_db_core::models::User> = users
        .iter()
        .filter(|user| added.contains(&user.id))
        .collect();
    announce(pool, broadcaster, target, &added_users).await;
    Ok(added)
}

/// Removes the users named in `usernames` in one transaction and records an `unassigned`
/// timeline event for each one that was assigned. Returns those names.
pub async fn unassign(
    pool: &codeyard_db_core::DbPool,
    target: &Target<'_>,
    authority: impl Into<crate::backend::db::credentials::Actor>,
    usernames: &[String],
) -> anyhow::Result<Vec<String>> {
    let removed = crate::backend::db::assignees::unassign_users(
        pool,
        authority,
        target.repository.id,
        target.item,
        usernames,
    )
    .await?;
    Ok(removed)
}

/// Announces assignments already saved with their timeline events.
pub async fn announce(
    pool: &codeyard_db_core::DbPool,
    broadcaster: &NotificationBroadcaster,
    target: &Target<'_>,
    users: &[&codeyard_db_core::models::User],
) {
    if users.is_empty() {
        return;
    }
    let pull_request = matches!(target.item, AssignedItem::PullRequest(_));
    let link = format!(
        "/{}/{}/{}/{}",
        target.owner,
        target.repository.name,
        if pull_request { "pulls" } else { "issues" },
        target.number
    );
    let ids: Vec<i64> = users.iter().map(|user| user.id).collect();
    notify_assignees(
        pool,
        broadcaster,
        &Assignment {
            actor: target.actor,
            repository: target.repository,
            owner: target.owner,
            repo: &target.repository.name,
            number: target.number,
            pull_request,
            title: target.title,
            link: &link,
        },
        &ids,
    )
    .await;
}
