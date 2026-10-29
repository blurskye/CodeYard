use super::{auth, issues, pagination::Pagination, repository, Error, Result};
use crate::backend::{
    app::AppState,
    db::{
        self,
        issue_attributes::{Refusal, ValueInput},
    },
    webhooks::payloads,
};
use axum::{
    body::Bytes,
    extract::{OriginalUri, Path, Query, State},
    http::{HeaderMap, Method, StatusCode},
    response::Response,
    routing::{delete, get, post, put, MethodRouter},
    Json,
};
use serde::Deserialize;
use serde_json::Value;
use std::{collections::HashMap, sync::Arc};
type Params = HashMap<String, String>;
pub(super) fn error(error: anyhow::Error) -> Error {
    match error.downcast_ref::<Refusal>() {
        Some(Refusal::Denied) => Error::new(StatusCode::FORBIDDEN, error.to_string()),
        Some(Refusal::Missing) => Error::missing(),
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
    let attributes = db::issue_attributes::catalog(
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
            .map(payloads::issue_type)
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
    let issue = issues::required_issue(&state.pool, repo.id, &p["issue_number"]).await?;
    let values = db::issue_attributes::values(
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
            .map(payloads::issue_field_value)
            .collect(),
        next,
    )
}
#[derive(Deserialize)]
struct Input {
    #[serde(default)]
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
    let actor = auth.ok_or_else(auth::required)?.user;
    let issue = issues::required_issue(&state.pool, repo.id, &p["issue_number"]).await?;
    let input: Input = serde_json::from_slice(&body)
        .map_err(|_| Error::validation("IssueFieldValue", "issue_field_values", "invalid"))?;
    if method == Method::PUT && input.issue_field_values.iter().any(|v| v.value.is_array()) {
        return Err(Error::validation("IssueFieldValue", "value", "invalid"));
    }
    db::issue_attributes::set_values(
        &state.pool,
        repo.id,
        actor.id,
        issue,
        &input.issue_field_values,
        method == Method::PUT,
    )
    .await
    .map_err(error)?;
    let values = db::issue_attributes::values(&state.pool, repo.id, issue, Some(actor.id))
        .await
        .map_err(Error::unavailable)?;
    Ok(Json(Value::Array(
        values.iter().map(payloads::issue_field_value).collect(),
    )))
}
async fn remove(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(p): Path<Params>,
) -> Result<StatusCode> {
    let (repo, auth) = repository(&state, &headers, &p["owner"], &p["repo"], true).await?;
    let actor = auth.ok_or_else(auth::required)?.user;
    let issue = issues::required_issue(&state.pool, repo.id, &p["issue_number"]).await?;
    let field = p["issue_field_id"].parse().map_err(|_| Error::missing())?;
    db::issue_attributes::delete_value(&state.pool, repo.id, actor.id, issue, field)
        .await
        .map_err(error)?;
    Ok(StatusCode::NO_CONTENT)
}
pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        ("repos/list-issue-types", get(types)),
        ("issues/list-issue-field-values-for-issue", get(list)),
        ("issues/add-issue-field-values", post(save)),
        ("issues/set-issue-field-values", put(save)),
        ("issues/delete-issue-field-value", delete(remove)),
    ]
}
