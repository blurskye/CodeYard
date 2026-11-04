//! Authorized browser destinations for discussion and discussion-comment URLs.

use crate::discussion::ResultPage;
use axum::extract::Path;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::http::Uri;
use axum::response::Response;
use axum::routing::get;
use axum::Router;
use codeyard_api_http::context::PageContext;
use codeyard_app::app::AppState;
use codeyard_webui::app::Page;
use codeyard_webui::pages::repos::discussions as view;
use codeyard_webui::pages::repos::page::Page as RepoPage;
use std::collections::HashMap;
use std::sync::Arc;

pub fn page_router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/:owner/:repo/discussions", get(index))
        .route("/:owner/:repo/discussions/:number", get(detail))
}
async fn names(ctx: &PageContext, mut ids: Vec<i64>) -> ResultPage<HashMap<i64, String>> {
    ids.sort_unstable();
    ids.dedup();
    Ok(
        codeyard_db_identity::users::accounts::get_users_by_ids(&ctx.state.pool, &ids)
            .await
            .map_err(|_| codeyard_api_http::shared::repos::chrome::unavailable(ctx))?
            .into_iter()
            .filter(|user| !user.is_banned)
            .map(|user| (user.id, user.username))
            .collect(),
    )
}
fn author(names: &HashMap<i64, String>, id: i64) -> String {
    names
        .get(&id)
        .cloned()
        .unwrap_or_else(|| "Deleted user".into())
}
fn window(ctx: &PageContext) -> ResultPage<(usize, i64)> {
    let page = crate::paging::number(ctx, "page", 1, 1..=crate::paging::MAX_PAGE)?;
    let (offset, _) = crate::paging::page_window(page, 30)
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
    let loaded =
        codeyard_api_http::shared::repos::chrome::load_database(&ctx, &owner, &name).await?;
    let (page, offset) = window(&ctx)?;
    let session = codeyard_api_http::session::extract_session_id(&ctx.headers).unwrap_or_default();
    let mut rows = codeyard_db_repos::repository_discussions::list_rows(
        &ctx.state.pool,
        ctx.user.as_ref().map_or(-1, |user| user.id),
        &codeyard_db_core::queries::users::accounts::AccountCredential::Session(&session),
        &codeyard_db_repos::repository_discussions::Filter {
            repo: Some(loaded.repository.id),
            order: codeyard_db_repos::repository_discussions::Order::Updated,
            ..Default::default()
        },
        offset,
        31,
    )
    .await
    .map_err(|_| codeyard_api_http::shared::repos::chrome::unavailable(&ctx))?;
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
    let current =
        codeyard_api_http::shared::repos::chrome::load_database(&ctx, &owner, &name).await?;
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
async fn comment(
    ctx: &PageContext,
    discussion: i64,
    id: i64,
) -> ResultPage<codeyard_db_repos::repository_discussions::Comment> {
    codeyard_db_repos::repository_discussions::comment(&ctx.state.pool, id)
        .await
        .map_err(|_| codeyard_api_http::shared::repos::chrome::unavailable(ctx))?
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
    let loaded =
        codeyard_api_http::shared::repos::chrome::load_database(&ctx, &owner, &name).await?;
    let discussion = codeyard_db_repos::repository_discussions::by_number(
        &ctx.state.pool,
        loaded.repository.id,
        number,
    )
    .await
    .map_err(|_| codeyard_api_http::shared::repos::chrome::unavailable(&ctx))?
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
    let (_, mut rows) = codeyard_db_repos::repository_discussions::comments(
        &ctx.state.pool,
        discussion.id,
        thread,
        offset,
        31,
    )
    .await
    .map_err(|_| codeyard_api_http::shared::repos::chrome::unavailable(&ctx))?;
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
    let current =
        codeyard_api_http::shared::repos::chrome::load_database(&ctx, &owner, &name).await?;
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
