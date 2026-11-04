use axum::extract::rejection::JsonRejection;
use axum::extract::MatchedPath;
use axum::extract::OriginalUri;
use axum::extract::Path;
use axum::extract::Query;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::response::Response;
use axum::routing::delete;
use axum::routing::get;
use axum::routing::patch;
use axum::routing::post;
use axum::routing::put;
use axum::routing::MethodRouter;
use axum::Json;
use axum::Router;
use codeyard_api_http::shared::github::pagination::Pagination;
use codeyard_app::app::AppState;
use codeyard_db_actions::environment_values as values;
use codeyard_db_actions::environment_values::EnvironmentValue;
use codeyard_db_actions::environment_values::Save;
use codeyard_db_actions::environment_values::ValueError;
use codeyard_db_orgs::organization_values::Kind;
use codeyard_model::github_error::Error;
use codeyard_model::github_error::Result;
use serde::Deserialize;
use serde_json::json;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        ("actions/list-environment-secrets", get(list)),
        ("actions/get-environment-public-key", get(public_key)),
        ("actions/get-environment-secret", get(value)),
        (
            "actions/create-or-update-environment-secret",
            put(save_secret),
        ),
        ("actions/delete-environment-secret", delete(remove)),
        ("actions/list-environment-variables", get(list)),
        ("actions/create-environment-variable", post(create_variable)),
        ("actions/get-environment-variable", get(value)),
        (
            "actions/update-environment-variable",
            patch(update_variable),
        ),
        ("actions/delete-environment-variable", delete(remove)),
    ]
}

pub(super) fn aliases() -> Router<Arc<AppState>> {
    Router::new()
        .route(
            "/repositories/:repository_id/environments/:environment_name/variables",
            post(create_variable),
        )
        .route(
            "/repositories/:repository_id/environments/:environment_name/variables/:name",
            patch(update_variable),
        )
}

fn error(error: ValueError) -> Error {
    match error {
        ValueError::Denied => Error::new(StatusCode::FORBIDDEN, "Resource not accessible by token"),
        ValueError::NotFound => Error::missing(),
        ValueError::Exists => Error::validation("Variable", "name", "already_exists"),
        ValueError::InvalidName => Error::validation("ActionsValue", "name", "invalid"),
        ValueError::InvalidEnvironment => Error::validation("Environment", "name", "invalid"),
        ValueError::InvalidValue
        | ValueError::Limit
        | ValueError::Size
        | ValueError::SecretsSize => Error::invalid(error.to_string()),
        _ => Error::unavailable(error),
    }
}
fn kind(route: &MatchedPath) -> Kind {
    if route.as_str().contains("/variables") {
        Kind::Variable
    } else {
        Kind::Secret
    }
}
fn name(params: &HashMap<String, String>) -> Result<String> {
    params
        .get("secret_name")
        .or_else(|| params.get("name"))
        .map(|name| name.to_ascii_uppercase())
        .ok_or_else(Error::missing)
}
async fn target(
    state: &AppState,
    headers: &HeaderMap,
    params: &HashMap<String, String>,
    write: bool,
) -> Result<(
    codeyard_db_core::models::Repository,
    codeyard_app::api_shared::github::auth::Authentication,
)> {
    let (owner, name) = if let Some(id) = params.get("repository_id") {
        let id = id.parse::<i64>().map_err(|_| Error::missing())?;
        let repo = codeyard_db_repos::repositories::records::get_repository_by_id(&state.pool, id)
            .await
            .map_err(Error::unavailable)?
            .ok_or_else(Error::missing)?;
        let owner =
            codeyard_db_repos::repositories::records::repository_owner_name(&state.pool, id)
                .await
                .map_err(Error::unavailable)?
                .ok_or_else(Error::missing)?;
        (owner, repo.name)
    } else {
        (params["owner"].clone(), params["repo"].clone())
    };
    let (repo, auth) =
        codeyard_app::api_shared::github::repository(state, headers, &owner, &name, false).await?;
    if params
        .get("repository_id")
        .is_some_and(|id| id.parse::<i64>().ok() != Some(repo.id))
    {
        return Err(Error::missing());
    }
    let auth = auth.ok_or_else(codeyard_app::api_shared::github::auth::required)?;
    if write {
        auth.require_write()?;
        values::list(
            &state.pool,
            auth.user.id,
            repo.id,
            &params["environment_name"],
            Kind::Secret,
        )
        .await
        .map_err(error)?;
    }
    Ok((repo, auth))
}
pub(super) fn base_document(
    kind: Kind,
    name: &str,
    value: &str,
    created_at: &chrono::DateTime<chrono::Utc>,
    updated_at: &chrono::DateTime<chrono::Utc>,
) -> Value {
    let mut body = json!({"name":name,"created_at":created_at,"updated_at":updated_at});
    if kind == Kind::Variable {
        body["value"] = json!(value);
    }
    body
}
fn document(kind: Kind, row: &EnvironmentValue) -> Value {
    base_document(
        kind,
        &row.name,
        &row.value,
        &row.created_at,
        &row.updated_at,
    )
}
async fn list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<HashMap<String, String>>,
    route: MatchedPath,
    OriginalUri(uri): OriginalUri,
    Query(page): Query<Pagination>,
) -> Result<Response> {
    let (repo, auth) = target(&state, &headers, &params, false).await?;
    let kind = kind(&route);
    let rows = values::list(
        &state.pool,
        auth.user.id,
        repo.id,
        &params["environment_name"],
        kind,
    )
    .await
    .map_err(error)?;
    let total = rows.len();
    let (offset, limit) = page.window()?;
    let items: Vec<_> = rows
        .into_iter()
        .skip(offset)
        .take(limit)
        .map(|row| document(kind, &row))
        .collect();
    let mut body = json!({"total_count":total});
    body[if kind == Kind::Secret {
        "secrets"
    } else {
        "variables"
    }] = json!(items);
    Ok((
        page.headers(&uri, offset.saturating_add(limit) < total)?,
        Json(body),
    )
        .into_response())
}
async fn value(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<HashMap<String, String>>,
    route: MatchedPath,
) -> Result<Json<Value>> {
    let (repo, auth) = target(&state, &headers, &params, false).await?;
    let kind = kind(&route);
    let name = name(&params)?;
    let row = values::list(
        &state.pool,
        auth.user.id,
        repo.id,
        &params["environment_name"],
        kind,
    )
    .await
    .map_err(error)?
    .into_iter()
    .find(|row| row.name == name)
    .ok_or_else(Error::missing)?;
    Ok(Json(document(kind, &row)))
}
async fn public_key(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<HashMap<String, String>>,
) -> Result<Json<Value>> {
    let (repo, auth) = target(&state, &headers, &params, false).await?;
    values::list(
        &state.pool,
        auth.user.id,
        repo.id,
        &params["environment_name"],
        Kind::Secret,
    )
    .await
    .map_err(error)?;
    let encryptor = state
        .secret_encryptor
        .as_deref()
        .ok_or_else(|| Error::unavailable(()))?;
    let (key_id, key) = codeyard_db_actions::secrets::repository_secret_key_for(
        &state.pool,
        codeyard_db_actions::secrets::SecretKind::Actions,
        encryptor,
        auth.user.id,
        repo.id,
        &auth.credential(),
    )
    .await
    .map_err(crate::actions::action_secrets::error)?;
    Ok(Json(crate::actions::action_secrets::public_key_document(
        &key_id, &key,
    )))
}

#[derive(Deserialize)]
struct SecretInput {
    encrypted_value: String,
    key_id: String,
}
async fn save_secret(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<HashMap<String, String>>,
    body: std::result::Result<Json<SecretInput>, JsonRejection>,
) -> Result<Response> {
    let (repo, auth) = target(&state, &headers, &params, true).await?;
    let Json(body) = body.map_err(|_| Error::invalid("Invalid secret input"))?;
    let encryptor = state
        .secret_encryptor
        .as_deref()
        .ok_or_else(|| Error::unavailable(()))?;
    let (id, key) = codeyard_db_actions::secrets::repository_secret_key_for(
        &state.pool,
        codeyard_db_actions::secrets::SecretKind::Actions,
        encryptor,
        auth.user.id,
        repo.id,
        &auth.credential(),
    )
    .await
    .map_err(crate::actions::action_secrets::error)?;
    let value =
        crate::actions::action_secrets::open_value(&key, &id, &body.key_id, &body.encrypted_value)?;
    let created = values::save(
        &state.pool,
        Some(encryptor),
        auth.database_actor(),
        repo.id,
        &params["environment_name"],
        &Save {
            kind: Kind::Secret,
            previous: None,
            name: &name(&params)?,
            value: Some(&value),
        },
    )
    .await
    .map_err(error)?;
    Ok(if created {
        (StatusCode::CREATED, Json(json!({}))).into_response()
    } else {
        StatusCode::NO_CONTENT.into_response()
    })
}
#[derive(Deserialize)]
struct CreateVariable {
    name: String,
    #[serde(default)]
    value: String,
}
async fn create_variable(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<HashMap<String, String>>,
    body: std::result::Result<Json<CreateVariable>, JsonRejection>,
) -> Result<Response> {
    let (repo, auth) = target(&state, &headers, &params, true).await?;
    let Json(body) = body.map_err(|_| Error::invalid("Invalid variable input"))?;
    values::save(
        &state.pool,
        None,
        auth.database_actor(),
        repo.id,
        &params["environment_name"],
        &Save {
            kind: Kind::Variable,
            previous: None,
            name: &body.name,
            value: Some(&body.value),
        },
    )
    .await
    .map_err(|failure| match failure {
        ValueError::Exists => Error::new(StatusCode::CONFLICT, "Variable already exists"),
        other => error(other),
    })?;
    Ok((StatusCode::CREATED, Json(json!({}))).into_response())
}
#[derive(Deserialize)]
struct UpdateVariable {
    name: Option<String>,
    value: Option<String>,
}
async fn update_variable(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<HashMap<String, String>>,
    body: std::result::Result<Json<UpdateVariable>, JsonRejection>,
) -> Result<StatusCode> {
    let (repo, auth) = target(&state, &headers, &params, true).await?;
    let Json(body) = body.map_err(|_| Error::invalid("Invalid variable input"))?;
    let previous = name(&params)?;
    let value = body
        .value
        .as_deref()
        .or_else(|| body.name.is_none().then_some(""));
    values::save(
        &state.pool,
        None,
        auth.database_actor(),
        repo.id,
        &params["environment_name"],
        &Save {
            kind: Kind::Variable,
            previous: Some(&previous),
            name: body.name.as_deref().unwrap_or(&previous),
            value,
        },
    )
    .await
    .map_err(error)?;
    Ok(StatusCode::NO_CONTENT)
}
async fn remove(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<HashMap<String, String>>,
    route: MatchedPath,
) -> Result<StatusCode> {
    let (repo, auth) = target(&state, &headers, &params, true).await?;
    values::delete(
        &state.pool,
        auth.database_actor(),
        repo.id,
        &params["environment_name"],
        kind(&route),
        &name(&params)?,
    )
    .await
    .map_err(error)?;
    Ok(StatusCode::NO_CONTENT)
}
