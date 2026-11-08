//! The Commits tab of a pull request: the commits its head has that its base does not.
use crate::discussion::item;
use crate::discussion::repository;
use crate::discussion::unavailable;
use crate::discussion::ResultPage;
use axum::extract::Path;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::http::Uri;
use axum::response::Response;
use codeyard_api_http::context::PageContext;
use codeyard_api_http::shared::repos::discussion::page;
use codeyard_app::app::AppState;
use codeyard_git_storage::repositories::store::repo_store;
use codeyard_webui::pages::repos::issues as view;
use codeyard_webui::pages::repos::issues::Kind;
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
    let pull = codeyard_db_repos::pull_requests::records::get_pull_request(
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
        .map(codeyard_api_http::shared::repos::chrome::commit)
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
