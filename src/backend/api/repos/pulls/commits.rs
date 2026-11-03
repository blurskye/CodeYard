//! The Commits tab of a pull request: the commits its head has that its base does not.
use crate::backend::api::http::context::PageContext;
use crate::backend::api::repos::chrome;
use crate::backend::api::repos::discussion::*;
use crate::backend::app::AppState;
use crate::backend::repositories::store::repo_store;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::Response;
use codeyard_webui::pages::repos::issues::{self as view, Kind};
use codeyard_webui::pages::repos::pulls::MAX_LISTED_COMMITS;
use std::sync::Arc;

pub(crate) async fn native_commits(
    State(state): State<Arc<AppState>>,
    Path((owner, name, number)): Path<(String, String, i32)>,
    headers: HeaderMap,
    uri: Uri,
) -> ResultPage<Response> {
    let ctx = PageContext::load(state, headers, &uri).await?;
    let repo = repository(&ctx, &owner, &name).await?;
    let value = item(&ctx, &repo, Kind::Pull, number).await?;
    let Some(compare) = value.comparison.as_ref() else {
        return Err(ctx.error(StatusCode::NOT_FOUND, "Pull request not found."));
    };
    // A retargeted stack layer lists only its own commits, from its saved fork point.
    let pull = crate::backend::db::pull_requests::records::get_pull_request(
        &ctx.state.pool,
        repo.record.id,
        number,
    )
    .await
    .map_err(|_| unavailable(&ctx))?
    .ok_or_else(|| ctx.error(StatusCode::NOT_FOUND, "Pull request not found."))?;
    let base = if pull.diff_base() != pull.base_sha {
        pull.diff_base()
    } else {
        &compare.base_sha
    };
    let commits = repo_store(&repo.record, ctx.state.cluster.as_ref())
        .commits_between(base, &compare.head_sha, MAX_LISTED_COMMITS)
        .await
        .map_err(|_| unavailable(&ctx))?
        .into_iter()
        .map(chrome::commit)
        .collect::<Vec<_>>();
    Ok(ctx.page_model(
        &format!("{} #{}", value.title, value.number),
        page(view::Page::PullCommits {
            repository: repo.view.clone(),
            item: value,
            commits,
        }),
    ))
}
