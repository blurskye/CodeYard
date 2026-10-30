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
    let found = references::candidates(texts, document, (owner, name));
    let pool = &ctx.state.pool;
    // Retain counters for legacy model compatibility; native links use explicit resolutions.
    let mut counted = vec![repository.id];
    let mut others = Vec::new();
    let mut readable = vec![(owner.to_owned(), name.to_owned(), repository.clone())];
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
            readable.push((other_owner.clone(), other_name.clone(), record.clone()));
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
        targets: Some(Vec::new()),
        users: Vec::new(),
        autolinks: db::repositories::autolinks::for_rendering(pool, repository.id)
            .await
            .map_err(|_| unavailable(ctx))?
            .into_iter()
            .map(autolink)
            .collect::<Vec<_>>()
            .into(),
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
    // Resolution is bounded by both candidate count and wall time. Unresolved prose stays intact.
    resolved.targets = Some(tokio::time::timeout(std::time::Duration::from_secs(2), async {
        let mut targets = Vec::new();
        for (owner, name, token) in found.targets {
            let Some((_, _, record)) = readable.iter().find(|(o, n, _)| o == &owner && n == &name) else { continue; };
            let base = crate::webui::repository_path(&owner, &name);
            let href = if let Some(number) = token.strip_prefix('#') {
                let Ok(number) = number.parse::<i32>() else { continue; };
                let issue = db::issues::get_issue(pool, record.id, number).await.ok().flatten().is_some();
                let pull = if issue { false } else { db::pull_requests::records::get_pull_request(pool, record.id, number).await.ok().flatten().is_some() };
                let moved = if issue || pull { false } else {
                    match db::issues::management::redirect(pool, record.id, number).await.ok().flatten() {
                        Some(issue) => match db::repositories::records::get_repository_by_id(pool, issue.repo_id).await.ok().flatten() {
                            Some(repository) => can_read(pool, ctx.user.as_ref(), &repository).await.unwrap_or(false),
                            None => false,
                        },
                        None => false,
                    }
                };
                if !issue && !pull && !moved { continue; }
                format!("{base}/issues/{number}")
            } else {
                let store = crate::backend::repositories::store::repo_store(record, ctx.state.cluster.as_ref());
                let Ok(commit) = store.commit(&token).await else { continue; };
                format!("{base}/commit/{}", commit.sha)
            };
            targets.push(references::Target { owner, repo: name, token, href });
        }
        targets
    }).await.unwrap_or_default());
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
