use crate::insights::open;
use crate::insights::show;
use axum::extract::Path;
use axum::extract::Query;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::http::Uri;
use axum::response::Response;
use codeyard_api_http::context::PageContext;
use codeyard_app::app::AppState;
use codeyard_git_storage::repositories::traffic::Refusal;
use codeyard_webui::pages::repos::insights::traffic as view;
use codeyard_webui::pages::repos::insights::Page as InsightsPage;
use serde::Deserialize;
use std::sync::Arc;

#[derive(Deserialize)]
pub(super) struct Period {
    per: Option<String>,
}
fn failure(ctx: &PageContext, error: anyhow::Error) -> Response {
    match error.downcast_ref::<Refusal>() {
        Some(Refusal::Missing) => ctx.error(StatusCode::NOT_FOUND, "Repository not found."),
        Some(Refusal::Denied) => ctx.error(StatusCode::FORBIDDEN, &error.to_string()),
        Some(Refusal::Period) => ctx.error(StatusCode::UNPROCESSABLE_ENTITY, &error.to_string()),
        None => codeyard_api_http::shared::repos::chrome::unavailable(ctx),
    }
}
fn series(data: codeyard_db_repos::repositories::traffic::Summary) -> view::Series {
    view::Series {
        count: data.totals.count,
        uniques: data.totals.uniques,
        buckets: data
            .buckets
            .into_iter()
            .map(|row| view::Bucket {
                timestamp: row.timestamp,
                count: row.count,
                uniques: row.uniques,
            })
            .collect(),
    }
}
pub(super) async fn page(
    State(state): State<Arc<AppState>>,
    Path((owner, name)): Path<(String, String)>,
    headers: HeaderMap,
    uri: Uri,
    Query(period): Query<Period>,
) -> Response {
    let (ctx, loaded) = match open(state, headers, &uri, &owner, &name).await {
        Ok(opened) => opened,
        Err(response) => return response,
    };
    let weekly = match codeyard_git_storage::repositories::traffic::weekly(period.per.as_deref()) {
        Ok(value) => value,
        Err(error) => return failure(&ctx, error),
    };
    let pool = &ctx.state.pool;
    let viewer = ctx.user.as_ref();
    let repo = &loaded.repository;
    let (views, clones, paths, referrers) = match tokio::try_join!(
        codeyard_git_storage::repositories::traffic::summary(
            pool,
            viewer,
            repo,
            codeyard_db_repos::repositories::traffic::Kind::Views,
            weekly
        ),
        codeyard_git_storage::repositories::traffic::summary(
            pool,
            viewer,
            repo,
            codeyard_db_repos::repositories::traffic::Kind::Clones,
            weekly
        ),
        codeyard_git_storage::repositories::traffic::paths(pool, viewer, repo),
        codeyard_git_storage::repositories::traffic::referrers(pool, viewer, repo)
    ) {
        Ok(data) => data,
        Err(error) => return failure(&ctx, error),
    };
    let page = view::Page {
        weekly,
        views: series(views),
        clones: series(clones),
        paths: paths
            .into_iter()
            .map(|row| view::Path {
                path: row.path,
                title: row.title,
                count: row.count,
                uniques: row.uniques,
            })
            .collect(),
        referrers: referrers
            .into_iter()
            .map(|row| view::Referrer {
                referrer: row.referrer,
                count: row.count,
                uniques: row.uniques,
            })
            .collect(),
    };
    show(
        &ctx,
        loaded,
        "Repository traffic",
        InsightsPage::Traffic {
            page: Box::new(page),
        },
    )
    .await
}
