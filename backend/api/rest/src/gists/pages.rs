use super::*;
use crate::gists::error;
use crate::gists::number;
use crate::gists::page_url;
use crate::gists::refusal;
use crate::gists::render_editor;
use crate::gists::InvalidInput;
use crate::gists::EDITOR_BYTES;
use crate::gists::PAGE_SIZE;
use anyhow::Result;
use axum::response::IntoResponse;
use codeyard_api_http::context::PageContext;
use codeyard_app::app::AppState;
use codeyard_app::repositories::store::repo_store;

/// Listed metadata is reused; storage still owns placement checks and pending-write recovery.
async fn cards(
    ctx: &PageContext,
    entries: Vec<codeyard_db_repos::gists::Gist>,
) -> Result<Vec<view::Card>> {
    use futures_util::{stream, StreamExt};
    let owners = codeyard_db_identity::users::accounts::get_users_by_ids(
        &ctx.state.pool,
        &entries.iter().map(|gist| gist.owner_id).collect::<Vec<_>>(),
    )
    .await?
    .into_iter()
    .map(|user| (user.id, user.username))
    .collect::<std::collections::HashMap<_, _>>();
    let repositories = codeyard_db_repos::gists::git_repositories(
        &ctx.state.pool,
        &entries.iter().map(|gist| gist.repo_id).collect::<Vec<_>>(),
    )
    .await?
    .into_iter()
    .map(|repo| (repo.id, repo))
    .collect::<std::collections::HashMap<_, _>>();
    let (owners, repositories) = (&owners, &repositories);
    let results = stream::iter(entries)
        .map(|entry| async move {
            let (Some(owner), Some(repository)) = (
                owners.get(&entry.owner_id),
                repositories.get(&entry.repo_id),
            ) else {
                return Ok::<_, anyhow::Error>(None);
            };
            let snapshot = repo_store(repository, ctx.state.cluster.as_ref())
                .gist_snapshot(&ctx.state.pool, entry.repo_id, None)
                .await;
            let (files, unavailable) = match snapshot {
                Ok(snapshot) => (snapshot.files, false),
                Err(failure) => match refusal(&failure.clone().into()).0 {
                    StatusCode::NOT_FOUND => return Ok(None),
                    StatusCode::SERVICE_UNAVAILABLE => (Vec::new(), true),
                    _ => return Err(failure.into()),
                },
            };
            Ok(Some(view::Card {
                id: entry.id,
                owner: owner.clone(),
                description: entry.description,
                public: entry.is_public,
                filenames: files
                    .iter()
                    .take(5)
                    .map(|entry| entry.path.clone())
                    .collect(),
                file_count: files.len(),
                files_unavailable: unavailable,
                comments: entry.comments,
                updated_at: entry.updated_at.to_rfc3339(),
            }))
        })
        .buffered(4)
        .collect::<Vec<_>>()
        .await;
    let mut cards = Vec::new();
    for result in results {
        if let Some(card) = result? {
            cards.push(card);
        }
    }
    Ok(cards)
}
async fn list_model(ctx: &PageContext) -> Result<view::Listing> {
    let tab = ctx
        .query
        .get("tab")
        .map(String::as_str)
        .unwrap_or(if ctx.user.is_some() { "mine" } else { "public" });
    let page = number(ctx, "page")?;
    let mut owner = None;
    let scope = if let Some(username) = ctx.query.get("user") {
        let user =
            codeyard_db_identity::users::accounts::get_user_by_username(&ctx.state.pool, username)
                .await?
                .filter(|user| !user.is_banned)
                .ok_or(codeyard_db_repos::gists::GistRefusal::Missing)?;
        owner = Some(user.username);
        codeyard_db_repos::gists::ListScope::Owner(user.id, false)
    } else {
        match tab {
            "public" => codeyard_db_repos::gists::ListScope::Public,
            "mine" => codeyard_db_repos::gists::ListScope::Owner(
                ctx.user
                    .as_ref()
                    .ok_or(codeyard_db_repos::gists::GistRefusal::Denied)?
                    .id,
                true,
            ),
            "starred" => codeyard_db_repos::gists::ListScope::Starred(
                ctx.user
                    .as_ref()
                    .ok_or(codeyard_db_repos::gists::GistRefusal::Denied)?
                    .id,
            ),
            _ => return Err(InvalidInput("Unknown gist list.").into()),
        }
    };
    if let codeyard_db_repos::gists::ListScope::Owner(owner, true) = scope {
        domain::recover_owned(&ctx.state, owner).await?;
    }
    let mut entries = codeyard_db_repos::gists::list_for_session(
        &ctx.state.pool,
        &codeyard_db_repos::gists::GistList {
            scope,
            since: None,
            limit: PAGE_SIZE as i64 + 1,
            offset: ((page - 1) * PAGE_SIZE) as i64,
        },
        &session::extract_session_id(&ctx.headers).unwrap_or_default(),
    )
    .await?;
    let more = entries.len() > PAGE_SIZE;
    entries.truncate(PAGE_SIZE);
    let cards = cards(ctx, entries).await?;
    Ok(view::Listing {
        cards,
        tab: if owner.is_some() { "user" } else { tab }.into(),
        owner,
        signed_in: ctx.user.is_some(),
        previous: (page > 1).then(|| page_url(ctx, "page", page - 1)),
        next: more.then(|| page_url(ctx, "page", page + 1)),
    })
}
pub(super) async fn list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    if ctx
        .query
        .get("tab")
        .is_some_and(|tab| matches!(tab.as_str(), "mine" | "starred"))
    {
        if let Err(response) = ctx.require_user() {
            return response;
        }
    }
    match list_model(&ctx).await {
        Ok(page) => ctx.page_model("Gists", AppPage::Gists(view::Page::List(page))),
        Err(failure) => error(&ctx, failure),
    }
}
pub(super) async fn detail_model(ctx: &PageContext, id: &str) -> Result<view::Detail> {
    let loaded = domain::load(
        &ctx.state,
        id,
        ctx.query.get("revision").map(String::as_str),
    )
    .await?;
    let store = repo_store(&loaded.repository, ctx.state.cluster.as_ref());
    let actor = ctx.user.as_ref().map(|user| user.id);
    let own = actor == Some(loaded.gist.owner_id);
    let owner = codeyard_db_identity::users::accounts::get_user_by_id(
        &ctx.state.pool,
        loaded.gist.owner_id,
    )
    .await?
    .ok_or(codeyard_db_repos::gists::GistRefusal::Missing)?;
    let mut files = Vec::new();
    let mut remaining = 256 * 1024;
    for entry in &loaded.snapshot.files {
        let size = entry.size.unwrap_or(0);
        let length = remaining.min(64 * 1024).min(size);
        let mut bytes = if length == 0 {
            Vec::new()
        } else {
            store
                .blob_range(
                    &loaded.snapshot.sha,
                    &entry.path,
                    codeyard_app::storage::range::ByteRange::Window {
                        offset: 0,
                        length: length as u64,
                    },
                )
                .await?
                .bytes
        };
        remaining -= bytes.len();
        if bytes.len() < size {
            if let Err(error) = std::str::from_utf8(&bytes) {
                if error.error_len().is_none() {
                    bytes.truncate(error.valid_up_to());
                }
            }
        }
        let content = (length > 0 || size == 0)
            .then(|| domain::text_preview(&bytes, length).map(str::to_owned))
            .flatten();
        let truncated =
            content.as_ref().is_some_and(|text| text.len() < size) || (length == 0 && size > 0);
        files.push(view::File {
            filename: entry.path.clone(),
            content,
            size,
            truncated,
            raw_url: format!(
                "/gists/{id}/raw/{}/{}",
                loaded.snapshot.sha,
                codeyard_webui::encode_component(&entry.path)
            ),
            image: codeyard_webui::raw_content_type(&entry.path, true).starts_with("image/"),
        });
    }
    let history_page = number(ctx, "history_page")?;
    let mut history = store
        .gist_history(
            &ctx.state.pool,
            loaded.repository.id,
            Some(&loaded.snapshot.sha),
            (history_page - 1) * PAGE_SIZE,
            PAGE_SIZE + 1,
        )
        .await?;
    let history_more = history.len() > PAGE_SIZE;
    history.truncate(PAGE_SIZE);
    let mut revisions = Vec::new();
    let authors = codeyard_app::repositories::reads::commit_authors(
        &ctx.state.pool,
        &history
            .iter()
            .map(|revision| revision.author_email.clone())
            .collect::<Vec<_>>(),
    )
    .await?;
    for revision in history {
        let author = authors.get(&revision.author_email);
        let timestamp = chrono::DateTime::from_timestamp(revision.timestamp, 0)
            .ok_or(InvalidInput("Invalid gist commit timestamp."))?;
        revisions.push(view::Revision {
            sha: revision.sha,
            author: author.map(|user| user.username.clone()),
            committed_at: timestamp.to_rfc3339(),
            additions: revision.additions,
            deletions: revision.deletions,
        });
    }
    let forks_page = number(ctx, "forks_page")?;
    let mut entries = codeyard_db_repos::gists::list(
        &ctx.state.pool,
        &codeyard_db_repos::gists::GistList {
            scope: codeyard_db_repos::gists::ListScope::Forks(loaded.repository.id),
            since: None,
            limit: 11,
            offset: ((forks_page - 1) * 10) as i64,
        },
    )
    .await?;
    let forks_more = entries.len() > 10;
    entries.truncate(10);
    let forks = cards(ctx, entries).await?;
    let comments_page = number(ctx, "comments_page")?;
    let mut entries = codeyard_db_repos::gists::comments(
        &ctx.state.pool,
        loaded.repository.id,
        PAGE_SIZE as i64 + 1,
        ((comments_page - 1) * PAGE_SIZE) as i64,
    )
    .await?;
    let comments_more = entries.len() > PAGE_SIZE;
    entries.truncate(PAGE_SIZE);
    let mut comments = Vec::new();
    for comment in entries {
        let author = match comment.user_id {
            Some(author) => {
                codeyard_db_identity::users::accounts::get_user_by_id(&ctx.state.pool, author)
                    .await?
                    .map(|user| user.username)
            }
            None => None,
        };
        let can_edit = actor.is_some() && comment.user_id == actor;
        comments.push(view::Comment {
            id: comment.id,
            author,
            body: comment.body,
            created_at: comment.created_at.to_rfc3339(),
            can_edit,
            can_delete: own || can_edit,
            edit_body: None,
        });
    }
    let parent = match loaded.gist.forked_from_id {
        Some(parent) => codeyard_db_repos::gists::by_repository(&ctx.state.pool, parent)
            .await?
            .map(|gist| gist.id),
        None => None,
    };
    let starred = match actor {
        Some(actor) => {
            codeyard_db_repos::gists::starred(&ctx.state.pool, actor, loaded.repository.id).await?
        }
        None => false,
    };
    Ok(view::Detail {
        id: id.into(),
        owner: owner.username,
        description: loaded.gist.description,
        public: loaded.gist.is_public,
        revision: loaded.snapshot.sha,
        historical: ctx.query.contains_key("revision"),
        parent,
        git_url: format!(
            "{}/gists/{id}.git",
            codeyard_model::http_config::http_clone_base().trim_end_matches('/')
        ),
        files,
        revisions,
        history_previous: (history_page > 1)
            .then(|| page_url(ctx, "history_page", history_page - 1)),
        history_next: history_more.then(|| page_url(ctx, "history_page", history_page + 1)),
        forks,
        forks_previous: (forks_page > 1).then(|| page_url(ctx, "forks_page", forks_page - 1)),
        forks_next: forks_more.then(|| page_url(ctx, "forks_page", forks_page + 1)),
        comments,
        comments_page,
        comments_previous: (comments_page > 1)
            .then(|| page_url(ctx, "comments_page", comments_page - 1)),
        comments_next: comments_more.then(|| page_url(ctx, "comments_page", comments_page + 1)),
        signed_in: actor.is_some(),
        own,
        starred,
        csrf_token: ctx.csrf_token.clone(),
        message: None,
        comment_body: String::new(),
    })
}
pub(super) fn detail_response(
    ctx: &PageContext,
    status: StatusCode,
    page: view::Detail,
) -> Response {
    let public = page.public;
    let title = if page.description.is_empty() {
        format!("Gist by {}", page.owner)
    } else {
        page.description.chars().take(100).collect()
    };
    let mut response = ctx.page_model_status(
        status,
        &title,
        AppPage::Gists(view::Page::Detail(Box::new(page))),
    );
    if !public {
        response.headers_mut().insert(
            "x-robots-tag",
            axum::http::HeaderValue::from_static("noindex, nofollow, noarchive"),
        );
    }
    response
}
// Issued gist IDs, even deleted ones, must never fall through to a legacy repository.
async fn legacy_target(
    ctx: &PageContext,
    id: &str,
    uri: &Uri,
    suffix: &str,
) -> Result<Option<String>> {
    if codeyard_db_repos::gists::is_identifier(&ctx.state.pool, id).await? {
        return Ok(None);
    }
    if let Some(repo) =
        codeyard_db_repos::repositories::records::get_repository(&ctx.state.pool, "gists", id)
            .await?
    {
        if codeyard_identity::repository_access::can_read(&ctx.state.pool, ctx.user.as_ref(), &repo)
            .await?
        {
            let mut target = format!(
                "{}{suffix}",
                codeyard_webui::repository_path("gists", &repo.name)
            );
            if let Some(query) = uri.query() {
                target.push('?');
                target.push_str(query);
            }
            return Ok(Some(target));
        }
    }
    Ok(None)
}
pub(super) async fn read(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    uri: Uri,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    match legacy_target(&ctx, &id, &uri, "").await {
        Ok(Some(target)) => return ctx.redirect(&target),
        Ok(None) => {}
        Err(failure) => return error(&ctx, failure),
    }
    match detail_model(&ctx, &id).await {
        Ok(page) => detail_response(&ctx, StatusCode::OK, page),
        Err(failure) => error(&ctx, failure),
    }
}
pub(super) async fn editor_model(ctx: &PageContext, id: &str) -> Result<view::Editor> {
    let actor = ctx
        .user
        .as_ref()
        .ok_or(codeyard_db_repos::gists::GistRefusal::Denied)?;
    let loaded = domain::load(&ctx.state, id, None).await?;
    anyhow::ensure!(
        loaded.gist.owner_id == actor.id,
        codeyard_db_repos::gists::GistRefusal::Denied
    );
    let store = repo_store(&loaded.repository, ctx.state.cluster.as_ref());
    let mut remaining = EDITOR_BYTES;
    let mut files = Vec::new();
    for entry in loaded.snapshot.files {
        let content = if entry.size.unwrap_or(0) <= remaining {
            let bytes = store.blob_by_id(&entry.sha).await?;
            domain::text_preview(&bytes, remaining).map(str::to_owned)
        } else {
            None
        };
        remaining -= content.as_ref().map_or(0, String::len);
        files.push(view::EditorFile {
            original: Some(entry.path.clone()),
            filename: entry.path,
            content,
            delete: false,
        });
    }
    Ok(view::Editor {
        id: Some(id.into()),
        description: loaded.gist.description,
        public: loaded.gist.is_public,
        expected: loaded.snapshot.sha,
        files,
        csrf_token: ctx.csrf_token.clone(),
        message: None,
    })
}
pub(super) async fn new(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    if let Err(response) = ctx.require_user() {
        return response;
    }
    render_editor(
        &ctx,
        StatusCode::OK,
        view::Editor {
            csrf_token: ctx.csrf_token.clone(),
            files: vec![view::EditorFile {
                filename: "gist.txt".into(),
                content: Some(String::new()),
                ..Default::default()
            }],
            ..Default::default()
        },
    )
}
pub(super) async fn edit(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    uri: Uri,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    if let Err(response) = ctx.require_user() {
        return response;
    }
    match editor_model(&ctx, &id).await {
        Ok(editor) => render_editor(&ctx, StatusCode::OK, editor),
        Err(failure) => error(&ctx, failure),
    }
}
pub(super) async fn raw(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((id, sha, filename)): Path<(String, String, String)>,
    uri: Uri,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    let suffix = format!(
        "/raw/{}/{}",
        codeyard_webui::encode_component(&sha),
        codeyard_webui::encode_component(&filename)
    );
    match legacy_target(&ctx, &id, &uri, &suffix).await {
        Ok(Some(target)) => return ctx.redirect(&target),
        Ok(None) => {}
        Err(failure) => return error(&ctx, failure),
    }
    let loaded = match domain::load(&ctx.state, &id, Some(&sha)).await {
        Ok(loaded) => loaded,
        Err(failure) => return error(&ctx, failure),
    };
    let Some(entry) = loaded
        .snapshot
        .files
        .iter()
        .find(|entry| entry.path == filename)
    else {
        return ctx.error(StatusCode::NOT_FOUND, "Gist file not found.");
    };
    let etag = format!("W/\"{}\"", entry.sha);
    if codeyard_api_http::static_files::not_modified(&ctx.headers, &etag) {
        return ctx.with_cookie((StatusCode::NOT_MODIFIED, [(header::ETAG, etag)]).into_response());
    }
    let (range, partial) = match codeyard_api_http::range::requested(&ctx.headers, None) {
        Ok(range) => range,
        Err(()) => {
            return ctx.error(
                StatusCode::RANGE_NOT_SATISFIABLE,
                "Byte range is not satisfiable.",
            )
        }
    };
    let Some(permit) =
        codeyard_api_http::middleware::wait_for_slot(&ctx.state.repository_downloads).await
    else {
        return ctx.error(StatusCode::SERVICE_UNAVAILABLE, "Gist downloads are busy.");
    };
    let blob = match repo_store(&loaded.repository, ctx.state.cluster.as_ref())
        .blob_range(&loaded.snapshot.sha, &filename, range)
        .await
    {
        Ok(blob) => blob,
        Err(failure) if failure.is_range_not_satisfiable() => {
            return ctx.error(
                StatusCode::RANGE_NOT_SATISFIABLE,
                "Byte range is not satisfiable.",
            )
        }
        Err(failure) => return error(&ctx, failure.into()),
    };
    let mut response =
        codeyard_api_repos::code::raw::raw_response(&filename, blob, partial, permit);
    response
        .headers_mut()
        .insert(header::ETAG, etag.parse().expect("Git blob validator"));
    response.headers_mut().insert(
        "x-robots-tag",
        axum::http::HeaderValue::from_static("noindex, nofollow, noarchive"),
    );
    response.headers_mut().insert(
        header::CONTENT_SECURITY_POLICY,
        axum::http::HeaderValue::from_static("default-src 'none'; sandbox"),
    );
    ctx.with_cookie(response)
}
