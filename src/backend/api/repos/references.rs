//! The references a page's Markdown may link, resolved for its viewer. The page model carries the
//! answer, so the server and the browser render the same links, and a repository the viewer
//! cannot read never reaches the page.

use crate::backend::api::context::PageContext;
use crate::backend::api::repos::pulls::pull::unavailable;
use crate::backend::db;
use crate::backend::repositories::access::can_read;
use crate::webui::components::references::{self, References};
use axum::response::Response;

/// The references `texts` make, as the viewer of `repository`, named `owner/name`, may link them.
/// `document` is the mode the page renders the texts with: a repository file is a document, and
/// anything else is a comment.
pub(crate) async fn resolve_references(
    ctx: &PageContext,
    repository: &db::models::Repository,
    owner: &str,
    name: &str,
    texts: &[&str],
    document: bool,
) -> Result<References, Response> {
    let found = references::candidates(texts, document, (owner, name));
    let pool = &ctx.state.pool;
    // The page's own counter is always read: a preview typed after the page loaded links `#N`
    // from it, even when the text the page started with has no number.
    let mut counted = vec![repository.id];
    let mut others = Vec::new();
    for (other_owner, other_name) in found.repositories {
        // A missing repository and one the viewer cannot read are treated alike: both are left
        // out, so their references render as plain text.
        let Some(record) =
            db::repositories::records::get_repository(pool, &other_owner, &other_name)
                .await
                .map_err(|_| unavailable(ctx))?
        else {
            continue;
        };
        if can_read(pool, ctx.user.as_ref(), &record)
            .await
            .map_err(|_| unavailable(ctx))?
        {
            counted.push(record.id);
            others.push((record.id, other_owner, other_name));
        }
    }
    let lasts = db::discussions::last_discussion_numbers(pool, &counted)
        .await
        .map_err(|_| unavailable(ctx))?;
    let last_number = |id: i64| {
        lasts
            .iter()
            .find(|(repo_id, _)| *repo_id == id)
            .map_or(0, |(_, last)| i32::try_from(*last).unwrap_or(0))
    };
    let mut resolved = References {
        owner: owner.to_owned(),
        repo: name.to_owned(),
        last_number: last_number(repository.id),
        users: Vec::new(),
        repositories: others
            .into_iter()
            .map(|(id, owner, name)| references::Repository {
                owner,
                name,
                last_number: last_number(id),
            })
            .collect(),
    };
    resolved
        .repositories
        .sort_by(|a, b| (&a.owner, &a.name).cmp(&(&b.owner, &b.name)));
    if !found.users.is_empty() {
        let users = db::users::accounts::get_users_by_usernames(pool, &found.users)
            .await
            .map_err(|_| unavailable(ctx))?;
        resolved.users = users.into_iter().map(|user| user.username).collect();
        resolved.users.sort();
    }
    Ok(resolved)
}

/// The references of a page that shows the user's submitted text again, such as a preview or a
/// rejected save. That text must survive the page, so a failed lookup links nothing rather than
/// failing the page.
pub(crate) async fn resolve_references_or_none(
    ctx: &PageContext,
    repository: &db::models::Repository,
    owner: &str,
    name: &str,
    texts: &[&str],
    document: bool,
) -> References {
    resolve_references(ctx, repository, owner, name, texts, document)
        .await
        .unwrap_or_default()
}
