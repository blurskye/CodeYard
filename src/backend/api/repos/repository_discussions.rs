//! Authorized browser destinations for discussion and discussion-comment URLs.
use super::{chrome, discussion::ResultPage, paging};
use crate::backend::{
    api::http::context::PageContext,
    app::AppState,
    db::{self, repository_discussions as store},
};
use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode, Uri},
    response::Response,
    routing::get,
    Router,
};
use codeyard_webui::{
    app::Page,
    pages::repos::{discussions as view, page::Page as RepoPage},
};
use std::{collections::HashMap, sync::Arc};

pub fn page_router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/:owner/:repo/discussions", get(index))
        .route("/:owner/:repo/discussions/:number", get(detail))
}
async fn names(ctx: &PageContext, mut ids: Vec<i64>) -> ResultPage<HashMap<i64, String>> {
    ids.sort_unstable();
    ids.dedup();
    Ok(db::users::accounts::get_users_by_ids(&ctx.state.pool, &ids)
        .await
        .map_err(|_| chrome::unavailable(ctx))?
        .into_iter()
        .filter(|user| !user.is_banned)
        .map(|user| (user.id, user.username))
        .collect())
}
fn author(names: &HashMap<i64, String>, id: i64) -> String {
    names
        .get(&id)
        .cloned()
        .unwrap_or_else(|| "Deleted user".into())
}
fn window(ctx: &PageContext) -> ResultPage<(usize, i64)> {
    let page = paging::number(ctx, "page", 1, 1..=paging::MAX_PAGE)?;
    let (offset, _) = paging::page_window(page, 30)
        .ok_or_else(|| ctx.error(StatusCode::BAD_REQUEST, "Invalid page."))?;
    Ok((page, offset as i64))
}
async fn index(
    State(state): State<Arc<AppState>>,
    Path((owner, name)): Path<(String, String)>,
    headers: HeaderMap,
    uri: Uri,
) -> ResultPage<Response> {
    let ctx = PageContext::load(state, headers, &uri).await?;
    let loaded = chrome::load_database(&ctx, &owner, &name).await?;
    let (page, offset) = window(&ctx)?;
    let session =
        crate::backend::api::http::session::extract_session_id(&ctx.headers).unwrap_or_default();
    let mut rows = store::list_rows(
        &ctx.state.pool,
        ctx.user.as_ref().map_or(-1, |user| user.id),
        &db::users::accounts::AccountCredential::Session(&session),
        &store::Filter {
            repo: Some(loaded.repository.id),
            order: store::Order::Updated,
            ..Default::default()
        },
        offset,
        31,
    )
    .await
    .map_err(|_| chrome::unavailable(&ctx))?;
    let more = rows.len() > 30;
    rows.truncate(30);
    let names = names(&ctx, rows.iter().map(|row| row.author_id).collect()).await?;
    let discussions = rows
        .into_iter()
        .map(|row| view::Discussion {
            number: row.number,
            title: row.title,
            body: String::new(),
            author: author(&names, row.author_id),
        })
        .collect();
    let current = chrome::load_database(&ctx, &owner, &name).await?;
    if current.repository.id != loaded.repository.id {
        return Err(ctx.error(StatusCode::NOT_FOUND, "Repository not found."));
    }
    Ok(ctx.page_model(
        "Discussions",
        Page::Repositories(RepoPage::Discussions {
            repo: current.chrome,
            page: view::Page::Index {
                discussions,
                page,
                more,
            },
        }),
    ))
}
async fn comment(ctx: &PageContext, discussion: i64, id: i64) -> ResultPage<store::Comment> {
    store::comment(&ctx.state.pool, id)
        .await
        .map_err(|_| chrome::unavailable(ctx))?
        .filter(|comment| comment.discussion_id == discussion)
        .ok_or_else(|| ctx.error(StatusCode::NOT_FOUND, "Comment not found."))
}
async fn detail(
    State(state): State<Arc<AppState>>,
    Path((owner, name, number)): Path<(String, String, i32)>,
    headers: HeaderMap,
    uri: Uri,
) -> ResultPage<Response> {
    let ctx = PageContext::load(state, headers, &uri).await?;
    let loaded = chrome::load_database(&ctx, &owner, &name).await?;
    let discussion = store::by_number(&ctx.state.pool, loaded.repository.id, number)
        .await
        .map_err(|_| chrome::unavailable(&ctx))?
        .ok_or_else(|| ctx.error(StatusCode::NOT_FOUND, "Discussion not found."))?;
    let target = |key: &str| -> ResultPage<Option<i64>> {
        ctx.query
            .get(key)
            .map(|value| {
                value
                    .parse::<i64>()
                    .ok()
                    .filter(|id| *id > 0)
                    .ok_or_else(|| ctx.error(StatusCode::BAD_REQUEST, "Invalid comment."))
            })
            .transpose()
    };
    let thread = target("thread")?;
    let (page, offset) = window(&ctx)?;
    let mut root = None;
    if let Some(id) = thread {
        let row = comment(&ctx, discussion.id, id).await?;
        if row.reply_to_id.is_some() {
            return Err(ctx.error(StatusCode::NOT_FOUND, "Thread not found."));
        }
        root = Some(row);
    }
    let (_, mut rows) = store::comments(&ctx.state.pool, discussion.id, thread, offset, 31)
        .await
        .map_err(|_| chrome::unavailable(&ctx))?;
    let more = rows.len() > 30;
    rows.truncate(30);
    if let Some(root) = root {
        rows.insert(0, root);
    }
    if let Some(id) = target("comment")? {
        if !rows.iter().any(|row| row.id == id) {
            let focused = comment(&ctx, discussion.id, id).await?;
            if let Some(parent) = focused.reply_to_id {
                if !rows.iter().any(|row| row.id == parent) {
                    rows.push(comment(&ctx, discussion.id, parent).await?);
                }
            }
            rows.push(focused);
        }
    }
    let mut ids: Vec<_> = rows.iter().map(|row| row.author_id).collect();
    ids.push(discussion.author_id);
    let names = names(&ctx, ids).await?;
    let comments = rows
        .into_iter()
        .map(|row| view::Comment {
            id: row.id,
            body: row.body,
            author: author(&names, row.author_id),
            reply_to: row.reply_to_id,
        })
        .collect();
    let title = discussion.title.clone();
    let discussion = view::Discussion {
        number,
        title: discussion.title,
        body: discussion.body,
        author: author(&names, discussion.author_id),
    };
    let current = chrome::load_database(&ctx, &owner, &name).await?;
    if current.repository.id != loaded.repository.id {
        return Err(ctx.error(StatusCode::NOT_FOUND, "Repository not found."));
    }
    Ok(ctx.page_model(
        &title,
        Page::Repositories(RepoPage::Discussions {
            repo: current.chrome,
            page: view::Page::Detail {
                discussion,
                comments,
                thread,
                page,
                more,
            },
        }),
    ))
}
