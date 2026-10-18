use super::blob::file_context;
use crate::backend::api::http::context::PageContext;
use crate::backend::api::repos::chrome::{
    date, is_branch, load, ref_names_under, resolve, store_error, Loaded,
};
use crate::backend::app::AppState;
use crate::backend::repositories::store::{repo_store, ErrorKind};
use crate::webui::pages::repos as view;
use axum::extract::{OriginalUri, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use std::collections::HashMap;
use std::sync::Arc;

async fn blame_page(ctx: &PageContext, loaded: &Loaded, tail: &str) -> Result<Response, Response> {
    let resolved = resolve(ctx, loaded, tail, true)?;
    if loaded.refs.is_empty() {
        return Err(ctx.error(StatusCode::NOT_FOUND, "Repository is empty."));
    }
    let store = repo_store(&loaded.repository, ctx.state.cluster.as_ref());
    let blame = match store.blame(&resolved.reference, &resolved.path).await {
        Ok(blame) => blame,
        // A file the store refuses to blame is a fact about the file, not an outage.
        Err(error) if error.kind == ErrorKind::Invalid && !error.is_response_too_large() => {
            return Err(ctx.repo_error(
                &loaded.chrome,
                StatusCode::UNPROCESSABLE_ENTITY,
                &error.message,
            ));
        }
        Err(error) => return Err(store_error(ctx, Some(&loaded.chrome), error)),
    };
    let (folders, latest_commit, permalink) =
        file_context(ctx, loaded, &store, &resolved.reference, &resolved.path).await?;
    let page = view::blame::BlamePage {
        deletable: is_branch(loaded, &resolved.reference),
        branches: ref_names_under(loaded, "refs/heads/"),
        tags: ref_names_under(loaded, "refs/tags/"),
        folders,
        latest_commit,
        permalink,
        reference: resolved.reference,
        path: resolved.path,
        content: blame.content,
        size: blame.size,
        hunks: blame
            .hunks
            .into_iter()
            .map(|hunk| view::blame::BlameHunk {
                first_line: hunk.first_line,
                line_count: hunk.line_count,
                sha: hunk.commit,
                author: hunk.author_name,
                author_date: date(hunk.author_time),
                summary: hunk.summary,
            })
            .collect(),
    };
    let title = page.path.clone();
    Ok(ctx.page_model(
        &title,
        crate::webui::app::Page::Repositories(view::page::Page::Blame {
            repo: loaded.chrome.clone(),
            page,
        }),
    ))
}

pub(in crate::backend::api::repos) async fn page(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    OriginalUri(uri): OriginalUri,
    Path(params): Path<HashMap<String, String>>,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    let loaded = match load(&ctx, &params["owner"], &params["repo"]).await {
        Ok(loaded) => loaded,
        Err(response) => return response,
    };
    let tail = params.get("ref_path").map(String::as_str).unwrap_or("");
    match blame_page(&ctx, &loaded, tail).await {
        Ok(response) | Err(response) => response,
    }
}
