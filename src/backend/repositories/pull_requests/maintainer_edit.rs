//! Personal-fork consent is scoped to one open pull request, never general Git write access.
use crate::backend::{
    db::{
        self,
        models::{PullRequest, Repository, User},
    },
    repositories::access,
};

/// A fork's ongoing publication grant follows the real contributor's live access.
/// Importing an issue's author must never borrow that historical user's authority.
pub(crate) async fn publication_authorized(
    pool: &db::DbPool,
    publisher: Option<i64>,
    head: &Repository,
) -> anyhow::Result<bool> {
    let Some(id) = publisher else {
        return Ok(false);
    };
    let Some(user) = db::users::accounts::get_user_by_id(pool, id).await? else {
        return Ok(false);
    };
    if user.is_banned || user.must_change_password {
        return Ok(false);
    }
    access::can_read(pool, Some(&user), head).await
}

pub(crate) async fn can_update_head(
    pool: &db::DbPool,
    actor: &User,
    base: &Repository,
    head: &Repository,
    pull: &PullRequest,
) -> anyhow::Result<bool> {
    if actor.is_banned
        || actor.must_change_password
        || base.is_archived
        || head.is_archived
        || pull.state != "open"
        || pull.repo_id != base.id
        || pull.head_repo_id != Some(head.id)
        || !access::can_read(pool, Some(actor), base).await?
    {
        return Ok(false);
    }
    if head.id != base.id && !publication_authorized(pool, pull.head_publisher_id, head).await? {
        return Ok(false);
    }
    if access::can_write(pool, actor, head).await? {
        return Ok(true);
    }
    // The grant covers this PR head only, including a private fork; it never
    // makes the repository generally readable or writable to the maintainer.
    if !pull.maintainer_edits_enabled()
        || head.forked_from_id != Some(base.id)
        || head.id == base.id
        || head.owner_kind != "user"
        || pull.maintainer_edit_granted_by_id != head.personal_owner_id()
    {
        return Ok(false);
    }
    let Some(grantor) = pull.maintainer_edit_granted_by_id else {
        return Ok(false);
    };
    let active = db::users::accounts::get_user_by_id(pool, grantor)
        .await?
        .is_some_and(|owner| !owner.is_banned && !owner.must_change_password);
    Ok(active && access::can_write(pool, actor, base).await?)
}
