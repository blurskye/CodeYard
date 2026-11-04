use crate::backend::api::http::context::PageContext;
use crate::backend::api::repos::insights::open;
use crate::backend::api::repos::insights::show;

use crate::backend::app::AppState;

use crate::backend::repositories::traffic::Refusal;
use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode, Uri},
    response::Response,
};
use codeyard_webui::pages::repos::insights::{traffic as view, Page as InsightsPage};
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
        None => crate::backend::api::repos::chrome::unavailable(ctx),
    }
}
fn series(data: crate::backend::repositories::traffic::Summary) -> view::Series {
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
    let weekly = match crate::backend::repositories::traffic::weekly(period.per.as_deref()) {
        Ok(value) => value,
        Err(error) => return failure(&ctx, error),
    };
    let pool = &ctx.state.pool;
    let viewer = ctx.user.as_ref();
    let repo = &loaded.repository;
    let (views, clones, paths, referrers) = match tokio::try_join!(
        crate::backend::repositories::traffic::summary(
            pool,
            viewer,
            repo,
            crate::backend::repositories::traffic::Kind::Views,
            weekly
        ),
        crate::backend::repositories::traffic::summary(
            pool,
            viewer,
            repo,
            crate::backend::repositories::traffic::Kind::Clones,
            weekly
        ),
        crate::backend::repositories::traffic::paths(pool, viewer, repo),
        crate::backend::repositories::traffic::referrers(pool, viewer, repo)
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
