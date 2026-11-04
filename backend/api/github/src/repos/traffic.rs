use axum::extract::Path;
use axum::extract::Query;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::routing::get;
use axum::routing::MethodRouter;
use axum::Json;
use codeyard_app::api_shared::github::repository;
use codeyard_app::app::AppState;
use codeyard_repos::traffic::Refusal;
use codeyard_db_repos::repositories::traffic::Kind;
use codeyard_model::github_error::Error;
use codeyard_model::github_error::Result;
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
    let data = codeyard_repos::traffic::summary(
        &state.pool,
        actor.as_ref().map(|actor| &actor.user),
        &repo,
        Kind::Views,
        codeyard_repos::traffic::weekly(period.per.as_deref()).map_err(failure)?,
    )
    .await
    .map_err(failure)?;
    Ok(Json(codeyard_webhooks::payloads::repository_traffic(
        Kind::Views,
        &data,
    )))
}
async fn clones(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
    Query(period): Query<Period>,
) -> Result<Json<Value>> {
    let (repo, actor) = repository(&state, &headers, &owner, &name, false).await?;
    let data = codeyard_repos::traffic::summary(
        &state.pool,
        actor.as_ref().map(|actor| &actor.user),
        &repo,
        Kind::Clones,
        codeyard_repos::traffic::weekly(period.per.as_deref()).map_err(failure)?,
    )
    .await
    .map_err(failure)?;
    Ok(Json(codeyard_webhooks::payloads::repository_traffic(
        Kind::Clones,
        &data,
    )))
}
async fn paths(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
) -> Result<Json<Value>> {
    let (repo, actor) = repository(&state, &headers, &owner, &name, false).await?;
    let data = codeyard_repos::traffic::paths(
        &state.pool,
        actor.as_ref().map(|actor| &actor.user),
        &repo,
    )
    .await
    .map_err(failure)?;
    Ok(Json(Value::Array(
        data.iter()
            .map(codeyard_webhooks::payloads::content_traffic)
            .collect(),
    )))
}
async fn referrers(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
) -> Result<Json<Value>> {
    let (repo, actor) = repository(&state, &headers, &owner, &name, false).await?;
    let data = codeyard_repos::traffic::referrers(
        &state.pool,
        actor.as_ref().map(|actor| &actor.user),
        &repo,
    )
    .await
    .map_err(failure)?;
    Ok(Json(Value::Array(
        data.iter()
            .map(codeyard_webhooks::payloads::referrer_traffic)
            .collect(),
    )))
}
pub(crate) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        ("repos/get-views", get(views)),
        ("repos/get-clones", get(clones)),
        ("repos/get-top-paths", get(paths)),
        ("repos/get-top-referrers", get(referrers)),
    ]
}
