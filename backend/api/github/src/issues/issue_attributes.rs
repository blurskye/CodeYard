use axum::body::Bytes;
use axum::extract::OriginalUri;
use axum::extract::Path;
use axum::extract::Query;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::Method;
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::delete;
use axum::routing::get;
use axum::routing::post;
use axum::routing::put;
use axum::routing::MethodRouter;
use axum::Json;
use codeyard_api_http::shared::github::pagination::Pagination;
use codeyard_app::api_shared::github::repository;
use codeyard_app::app::AppState;
use codeyard_db_repos::issue_attributes::Refusal;
use codeyard_db_repos::issue_attributes::ValueInput;
use codeyard_model::github_error::Error;
use codeyard_model::github_error::Result;
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

type Params = HashMap<String, String>;
pub(crate) fn error(error: anyhow::Error) -> Error {
    match error.downcast_ref::<Refusal>() {
        Some(Refusal::Denied) => Error::new(StatusCode::FORBIDDEN, error.to_string()),
        Some(Refusal::Missing) => Error::missing(),
        Some(Refusal::Conflict) => Error::new(StatusCode::CONFLICT, error.to_string()),
        Some(_) => Error::invalid(error.to_string()),
        None if error.downcast_ref::<sqlx::Error>().is_some_and(|e| {
            e.as_database_error()
                .is_some_and(|d| d.is_unique_violation())
        }) =>
        {
            Error::invalid("Duplicate issue attribute")
        }
        None => Error::unavailable(error),
    }
}
async fn types(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(p): Path<Params>,
) -> Result<Json<Value>> {
    let (repo, auth) = repository(&state, &headers, &p["owner"], &p["repo"], false).await?;
    let attributes = codeyard_db_repos::issue_attributes::catalog(
        &state.pool,
        repo.id,
        auth.as_ref().map(|a| a.user.id),
        "type",
    )
    .await
    .map_err(Error::unavailable)?;
    Ok(Json(Value::Array(
        attributes
            .iter()
            .filter(|a| a.is_enabled)
            .map(codeyard_webhooks::payloads::issue_type)
            .collect(),
    )))
}
async fn list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(p): Path<Params>,
    Query(page): Query<Pagination>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let (repo, auth) = repository(&state, &headers, &p["owner"], &p["repo"], false).await?;
    let issue = crate::issues::required_issue(&state.pool, repo.id, &p["issue_number"]).await?;
    let values = codeyard_db_repos::issue_attributes::values(
        &state.pool,
        repo.id,
        issue,
        auth.as_ref().map(|a| a.user.id),
    )
    .await
    .map_err(Error::unavailable)?;
    let (offset, limit) = page.window()?;
    let next = values.len() > offset.saturating_add(limit);
    page.response(
        &uri,
        values
            .iter()
            .skip(offset)
            .take(limit)
            .map(codeyard_webhooks::payloads::issue_field_value)
            .collect(),
        next,
    )
}
#[derive(Deserialize)]
struct Input {
    issue_field_values: Vec<ValueInput>,
}
async fn save(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(p): Path<Params>,
    method: Method,
    body: Bytes,
) -> Result<Json<Value>> {
    let (repo, auth) = repository(&state, &headers, &p["owner"], &p["repo"], true).await?;
    let actor = auth.ok_or_else(codeyard_identity::github_auth::required)?;
    let issue = crate::issues::required_issue(&state.pool, repo.id, &p["issue_number"]).await?;
    let input: Input = serde_json::from_slice(&body)
        .map_err(|_| Error::validation("IssueFieldValue", "issue_field_values", "invalid"))?;
    codeyard_db_repos::issue_attributes::set_values(
        &state.pool,
        repo.id,
        actor.database_actor(),
        issue,
        &input.issue_field_values,
        method == Method::PUT,
    )
    .await
    .map_err(error)?;
    let values = codeyard_db_repos::issue_attributes::values(
        &state.pool,
        repo.id,
        issue,
        Some(actor.user.id),
    )
    .await
    .map_err(Error::unavailable)?;
    Ok(Json(Value::Array(
        values
            .iter()
            .map(codeyard_webhooks::payloads::issue_field_value)
            .collect(),
    )))
}
async fn remove(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(p): Path<Params>,
) -> Result<StatusCode> {
    let (repo, auth) = repository(&state, &headers, &p["owner"], &p["repo"], true).await?;
    let actor = auth.ok_or_else(codeyard_identity::github_auth::required)?;
    let issue = crate::issues::required_issue(&state.pool, repo.id, &p["issue_number"]).await?;
    let field = p["issue_field_id"].parse().map_err(|_| Error::missing())?;
    codeyard_db_repos::issue_attributes::delete_value(
        &state.pool,
        repo.id,
        actor.database_actor(),
        issue,
        field,
    )
    .await
    .map_err(error)?;
    Ok(StatusCode::NO_CONTENT)
}
pub(crate) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        ("repos/list-issue-types", get(types)),
        ("issues/list-issue-field-values-for-issue", get(list)),
        ("issues/add-issue-field-values", post(save)),
        ("issues/set-issue-field-values", put(save)),
        ("issues/delete-issue-field-value", delete(remove)),
    ]
}
