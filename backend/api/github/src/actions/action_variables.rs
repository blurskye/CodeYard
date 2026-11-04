use axum::extract::rejection::JsonRejection;
use axum::extract::OriginalUri;
use axum::extract::Path;
use axum::extract::Query;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::delete;
use axum::routing::get;
use axum::routing::patch;
use axum::routing::post;
use axum::routing::MethodRouter;
use axum::Json;
use codeyard_api_http::shared::github::pagination::Pagination;
use codeyard_app::app::AppState;
use codeyard_model::github_error::Error;
use codeyard_model::github_error::Result;
use serde::Deserialize;
use serde_json::json;
use serde_json::Value;
use std::sync::Arc;

pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        ("actions/list-repo-variables", get(list)),
        ("actions/create-repo-variable", post(create)),
        ("actions/get-repo-variable", get(variable)),
        ("actions/update-repo-variable", patch(update)),
        ("actions/delete-repo-variable", delete(remove)),
    ]
}

fn error(error: codeyard_db_actions::variables::VariableError) -> Error {
    use codeyard_db_actions::variables::VariableError;
    match error {
        VariableError::Denied => {
            Error::new(StatusCode::FORBIDDEN, "Resource not accessible by token")
        }
        VariableError::NotFound => Error::missing(),
        VariableError::Exists => Error::validation("Variable", "name", "already_exists"),
        VariableError::InvalidName => Error::validation("Variable", "name", "invalid"),
        VariableError::InvalidValue | VariableError::Limit => Error::invalid(error.to_string()),
        _ => Error::unavailable(error),
    }
}

async fn repository(
    state: &AppState,
    headers: &HeaderMap,
    owner: &str,
    name: &str,
    write: bool,
) -> Result<(i64, codeyard_app::api_shared::github::auth::Authentication)> {
    let (repo, auth) =
        codeyard_app::api_shared::github::repository(state, headers, owner, name, write).await?;
    Ok((
        repo.id,
        auth.ok_or_else(codeyard_app::api_shared::github::auth::required)?,
    ))
}

async fn list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
    OriginalUri(uri): OriginalUri,
    Query(page): Query<Pagination>,
) -> Result<Response> {
    let (repo, actor) = repository(&state, &headers, &owner, &name, false).await?;
    let variables =
        codeyard_db_actions::variables::list(&state.pool, actor.user.id, &actor.credential(), repo)
            .await
            .map_err(error)?;
    let total = variables.len();
    let (offset, limit) = page.window()?;
    let variables: Vec<_> = variables.into_iter().skip(offset).take(limit).collect();
    Ok((
        page.headers(&uri, offset.saturating_add(limit) < total)?,
        Json(json!({"total_count":total, "variables":variables})),
    )
        .into_response())
}

async fn variable(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo, name)): Path<(String, String, String)>,
) -> Result<Json<Value>> {
    let (repo, actor) = repository(&state, &headers, &owner, &repo, false).await?;
    let name = name.to_ascii_uppercase();
    let variable =
        codeyard_db_actions::variables::list(&state.pool, actor.user.id, &actor.credential(), repo)
            .await
            .map_err(error)?
            .into_iter()
            .find(|variable| variable.name == name)
            .ok_or_else(Error::missing)?;
    Ok(Json(json!(variable)))
}

#[derive(Deserialize)]
struct Create {
    name: String,
    #[serde(default)]
    value: String,
}

async fn create(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
    body: std::result::Result<Json<Create>, JsonRejection>,
) -> Result<Response> {
    let (repo, actor) = repository(&state, &headers, &owner, &name, true).await?;
    let Json(body) = body.map_err(|_| Error::invalid("Invalid variable input"))?;
    codeyard_db_actions::variables::save(
        &state.pool,
        actor.user.id,
        &actor.credential(),
        repo,
        None,
        &body.name.to_ascii_uppercase(),
        Some(&body.value),
    )
    .await
    .map_err(|failure| match failure {
        codeyard_db_actions::variables::VariableError::Exists => {
            Error::new(StatusCode::CONFLICT, "Variable already exists")
        }
        other => error(other),
    })?;
    Ok((StatusCode::CREATED, Json(json!({}))).into_response())
}

#[derive(Deserialize)]
struct Update {
    name: Option<String>,
    value: Option<String>,
}

async fn update(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo, previous)): Path<(String, String, String)>,
    body: std::result::Result<Json<Update>, JsonRejection>,
) -> Result<StatusCode> {
    let (repo, actor) = repository(&state, &headers, &owner, &repo, true).await?;
    let Json(body) = body.map_err(|_| Error::invalid("Invalid variable input"))?;
    let previous = previous.to_ascii_uppercase();
    // gh omits an empty value; name-only patches still retain the stored value.
    let value = body
        .value
        .as_deref()
        .or_else(|| body.name.is_none().then_some(""));
    let name = body
        .name
        .unwrap_or_else(|| previous.clone())
        .to_ascii_uppercase();
    codeyard_db_actions::variables::save(
        &state.pool,
        actor.user.id,
        &actor.credential(),
        repo,
        Some(&previous),
        &name,
        value,
    )
    .await
    .map_err(error)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn remove(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, repo, name)): Path<(String, String, String)>,
) -> Result<StatusCode> {
    let (repo, actor) = repository(&state, &headers, &owner, &repo, true).await?;
    codeyard_db_actions::variables::delete(
        &state.pool,
        actor.user.id,
        &actor.credential(),
        repo,
        &name.to_ascii_uppercase(),
    )
    .await
    .map_err(error)?;
    Ok(StatusCode::NO_CONTENT)
}
