//! The references a page's Markdown may link, resolved for its viewer. The page model carries the
//! answer, so the server and the browser render the same links, and a repository the viewer
//! cannot read never reaches the page.

use crate::backend::api::http::context::PageContext;
use crate::backend::api::repos::discussion::unavailable;
use crate::backend::db;
use crate::backend::repositories::access::can_read;
use crate::webui::components::references::{self, References};
use axum::response::Response;

pub(crate) fn autolink(row: db::repositories::autolinks::Autolink) -> references::Autolink {
    references::Autolink {
        id: row.id,
        key_prefix: row.key_prefix,
        url_template: row.url_template,
        is_alphanumeric: row.is_alphanumeric,
    }
}

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
    resolve_references_with(&ctx.state.pool, repository, owner, name, texts, document, |records| async move {
        let mut readable = std::collections::HashSet::new();
        for record in records {
            if can_read(&ctx.state.pool, ctx.user.as_ref(), &record).await? { readable.insert(record.id); }
        }
        Ok(readable)
    }).await.map_err(|_| unavailable(ctx))
}

/// The bounded native resolver with a caller-owned, credential-aware repository audience.
pub(crate) async fn resolve_references_with<F, Fut>(
    pool: &db::DbPool,
    repository: &db::models::Repository,
    owner: &str,
    name: &str,
    texts: &[&str],
    document: bool,
    authorize: F,
) -> anyhow::Result<References>
where
    F: Fn(Vec<db::models::Repository>) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<std::collections::HashSet<i64>>>,
{
    let sources: Vec<String> = texts.iter().map(|text| (*text).to_owned()).collect();
    let current = (owner.to_owned(), name.to_owned());
    let found = tokio::task::spawn_blocking(move || {
        let texts: Vec<&str> = sources.iter().map(String::as_str).collect();
        references::candidates(&texts, document, (&current.0, &current.1))
    }).await?;
    let mut records = vec![repository.clone()];
    let mut names = Vec::new();
    for (owner, name) in found.repositories {
        if let Some(record) = db::repositories::records::get_repository(pool, &owner, &name).await? {
            names.push((record.id, owner, name));
            records.push(record);
        }
    }
    let readable = authorize(records.clone()).await?;
    if !readable.contains(&repository.id) { return Ok(References::default()); }
    // The page's own counter is always read: a preview typed after the page loaded links `#N`
    // from it, even when the text the page started with has no number.
    let mut counted = vec![repository.id];
    let mut others = Vec::new();
    for (id, owner, name) in names {
        if readable.contains(&id) { counted.push(id); others.push((id, owner, name)); }
    }
    let lasts = db::discussions::last_discussion_numbers(pool, &counted).await?;
    let autolinks = db::repositories::autolinks::for_rendering(pool, repository.id).await?;
    // Do not let reference metadata read after a revocation escape through earlier decisions.
    let readable = authorize(records).await?;
    if !readable.contains(&repository.id) { return Ok(References::default()); }
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
        autolinks: autolinks.into_iter()
            .map(autolink)
            .collect::<Vec<_>>()
            .into(),
        repositories: others
            .into_iter()
            .filter(|(id, _, _)| readable.contains(id))
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
        let users = db::users::accounts::get_users_by_usernames(pool, &found.users).await?;
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
