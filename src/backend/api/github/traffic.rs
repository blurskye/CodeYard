use crate::backend::api::github::repository;
use crate::backend::api::github::Error;
use crate::backend::api::github::Result;
use crate::backend::app::AppState;

use crate::backend::repositories::traffic::Kind;
use crate::backend::repositories::traffic::Refusal;

use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    routing::{get, MethodRouter},
    Json,
};
use serde::Deserialize;
use serde_json::Value;
use std::sync::Arc;
#[derive(Deserialize)]
struct Period {
    per: Option<String>,
}
fn failure(error: anyhow::Error) -> Error {
    match error.downcast_ref::<Refusal>() {
        Some(Refusal::Missing) => Error::missing(),
        Some(Refusal::Denied) => Error::new(StatusCode::FORBIDDEN, error.to_string()),
        Some(Refusal::Period) => Error::invalid(error.to_string()),
        None => Error::unavailable(error),
    }
}
async fn views(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
    Query(period): Query<Period>,
) -> Result<Json<Value>> {
    let (repo, actor) = repository(&state, &headers, &owner, &name, false).await?;
    let data = crate::backend::repositories::traffic::summary(
        &state.pool,
        actor.as_ref().map(|actor| &actor.user),
        &repo,
        Kind::Views,
        crate::backend::repositories::traffic::weekly(period.per.as_deref()).map_err(failure)?,
    )
    .await
    .map_err(failure)?;
    Ok(Json(
        crate::backend::webhooks::payloads::repository_traffic(Kind::Views, &data),
    ))
}
async fn clones(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
    Query(period): Query<Period>,
) -> Result<Json<Value>> {
    let (repo, actor) = repository(&state, &headers, &owner, &name, false).await?;
    let data = crate::backend::repositories::traffic::summary(
        &state.pool,
        actor.as_ref().map(|actor| &actor.user),
        &repo,
        Kind::Clones,
        crate::backend::repositories::traffic::weekly(period.per.as_deref()).map_err(failure)?,
    )
    .await
    .map_err(failure)?;
    Ok(Json(
        crate::backend::webhooks::payloads::repository_traffic(Kind::Clones, &data),
    ))
}
async fn paths(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
) -> Result<Json<Value>> {
    let (repo, actor) = repository(&state, &headers, &owner, &name, false).await?;
    let data = crate::backend::repositories::traffic::paths(
        &state.pool,
        actor.as_ref().map(|actor| &actor.user),
        &repo,
    )
    .await
    .map_err(failure)?;
    Ok(Json(Value::Array(
        data.iter()
            .map(crate::backend::webhooks::payloads::content_traffic)
            .collect(),
    )))
}
async fn referrers(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
) -> Result<Json<Value>> {
    let (repo, actor) = repository(&state, &headers, &owner, &name, false).await?;
    let data = crate::backend::repositories::traffic::referrers(
        &state.pool,
        actor.as_ref().map(|actor| &actor.user),
        &repo,
    )
    .await
    .map_err(failure)?;
    Ok(Json(Value::Array(
        data.iter()
            .map(crate::backend::webhooks::payloads::referrer_traffic)
            .collect(),
    )))
}
pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        ("repos/get-views", get(views)),
        ("repos/get-clones", get(clones)),
        ("repos/get-top-paths", get(paths)),
        ("repos/get-top-referrers", get(referrers)),
    ]
}
