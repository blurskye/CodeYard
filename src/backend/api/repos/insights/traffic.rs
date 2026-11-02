use super::{open, show};
use crate::backend::{
    api::{http::context::PageContext, repos::chrome},
    app::AppState,
    repositories::traffic::{self, Refusal},
};
use crate::webui::pages::repos::insights::{traffic as view, Page as InsightsPage};
use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode, Uri},
    response::Response,
};
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
        None => chrome::unavailable(ctx),
    }
}
fn series(data: traffic::Summary) -> view::Series {
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
    let weekly = match traffic::weekly(period.per.as_deref()) {
        Ok(value) => value,
        Err(error) => return failure(&ctx, error),
    };
    let pool = &ctx.state.pool;
    let viewer = ctx.user.as_ref();
    let repo = &loaded.repository;
    let (views, clones, paths, referrers) = match tokio::try_join!(
        traffic::summary(pool, viewer, repo, traffic::Kind::Views, weekly),
        traffic::summary(pool, viewer, repo, traffic::Kind::Clones, weekly),
        traffic::paths(pool, viewer, repo),
        traffic::referrers(pool, viewer, repo)
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
