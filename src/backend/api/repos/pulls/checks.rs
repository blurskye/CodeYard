//! The Checks tab of a pull request: the workflow runs on its head commit.
use crate::backend::api::ci::jobs::collection::map_run;
use crate::backend::api::http::context::PageContext;
use crate::backend::api::repos::pulls::pull::*;
use crate::backend::app::AppState;
use crate::backend::db;
use crate::webui::pages::repos::issues::{self as view, Kind};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::Response;
use std::sync::Arc;

pub(crate) async fn native_checks(
    State(state): State<Arc<AppState>>,
    Path((owner, name, number)): Path<(String, String, i32)>,
    headers: HeaderMap,
    uri: Uri,
) -> ResultPage<Response> {
    let ctx = PageContext::load(state, headers, &uri).await?;
    let repo = repository(&ctx, &owner, &name).await?;
    let value = item(&ctx, &repo, Kind::Pull, number).await?;
    let pull =
        db::pull_requests::records::get_pull_request(&ctx.state.pool, repo.record.id, number)
            .await
            .map_err(|_| unavailable(&ctx))?
            .ok_or_else(|| ctx.error(StatusCode::NOT_FOUND, "Pull request not found."))?;
    let runs =
        db::ci::latest_ci_runs_for_commit(&ctx.state.pool, repo.record.id, &pull.head_sha, pull.id)
            .await
            .map_err(|_| unavailable(&ctx))?
            .into_iter()
            .map(|run| map_run(run, &repo.record))
            .collect::<Vec<_>>();
    Ok(ctx.page_model(
        &format!("{} #{}", value.title, value.number),
        page(view::Page::PullChecks {
            repository: repo.view.clone(),
            item: value,
            runs,
        }),
    ))
}
