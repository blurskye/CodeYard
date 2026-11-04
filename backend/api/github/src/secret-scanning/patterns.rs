use crate::secret_scanning::error as alert_error;
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
use axum::routing::MethodRouter;
use axum::Json;
use codeyard_app::api_shared::github::repository;
use codeyard_app::app::AppState;
use codeyard_model::github_error::Error;
use codeyard_model::github_error::Result;
use codeyard_support::secret_detection::patterns::Create;
use codeyard_support::secret_detection::patterns::Delete;
use codeyard_support::secret_detection::patterns::Owner;
use codeyard_support::secret_detection::patterns::Update;
use serde::Deserialize;
use serde_json::json;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

fn error(error: anyhow::Error) -> Error {
    if matches!(
        error.downcast_ref::<codeyard_db_core::queries::secret_scanning::Refusal>(),
        Some(codeyard_db_core::queries::secret_scanning::Refusal::Invalid)
    ) {
        Error::validation("SecretScanningCustomPattern", "pattern", "invalid")
    } else {
        alert_error(error)
    }
}

pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        ("secret-scanning/list-org-custom-patterns", get(list)),
        (
            "secret-scanning/bulk-create-org-custom-patterns",
            post(create),
        ),
        (
            "secret-scanning/bulk-delete-org-custom-patterns",
            delete(remove),
        ),
        ("secret-scanning/update-org-custom-pattern", patch(update)),
        ("secret-scanning/list-repo-custom-patterns", get(list)),
        (
            "secret-scanning/bulk-create-repo-custom-patterns",
            post(create),
        ),
        (
            "secret-scanning/bulk-delete-repo-custom-patterns",
            delete(remove),
        ),
        ("secret-scanning/update-repo-custom-pattern", patch(update)),
        ("secret-scanning/list-org-pattern-configs", get(configs)),
        (
            "secret-scanning/update-org-pattern-configs",
            patch(configure),
        ),
    ]
}
async fn scope(
    state: &AppState,
    headers: &HeaderMap,
    params: &HashMap<String, String>,
    write: bool,
) -> Result<(
    Owner,
    codeyard_identity::github_auth::Authentication,
)> {
    if let Some(org) = params.get("org") {
        let actor = codeyard_identity::github_auth::authenticate(state, headers, true)
            .await?
            .ok_or_else(codeyard_identity::github_auth::required)?;
        if write {
            actor.require_write()?;
        }
        let org = crate::orgs::target(state, org).await?;
        Ok((Owner::Organization(org.id), actor))
    } else {
        let (repo, actor) =
            repository(state, headers, &params["owner"], &params["repo"], write).await?;
        Ok((
            Owner::Repository(repo.id),
            actor.ok_or_else(codeyard_identity::github_auth::required)?,
        ))
    }
}
#[derive(Deserialize)]
struct Filters {
    state: Option<String>,
    push_protection: Option<String>,
    sort: Option<String>,
    direction: Option<String>,
}
async fn list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<HashMap<String, String>>,
    OriginalUri(uri): OriginalUri,
    Query(query): Query<Filters>,
    Query(pagination): Query<crate::pagination::CursorPagination>,
) -> Result<Response> {
    let (owner, actor) = scope(&state, &headers, &params, false).await?;
    if query
        .state
        .as_deref()
        .is_some_and(|state| !matches!(state, "published" | "unpublished"))
        || query
            .push_protection
            .as_deref()
            .is_some_and(|setting| !matches!(setting, "enabled" | "disabled"))
        || query
            .sort
            .as_deref()
            .is_some_and(|sort| !matches!(sort, "created" | "updated" | "name"))
        || query
            .direction
            .as_deref()
            .is_some_and(|direction| !matches!(direction, "asc" | "desc"))
    {
        return Err(Error::invalid("Invalid custom pattern filter"));
    }
    let mut patterns =
        codeyard_db_security::secret_scanning::list_patterns(&state.pool, actor.user.id, owner)
            .await
            .map_err(error)?;
    patterns.retain(|pattern| {
        query
            .state
            .as_ref()
            .is_none_or(|state| *state == pattern.state)
            && query
                .push_protection
                .as_deref()
                .is_none_or(|setting| (setting == "enabled") == pattern.push_protection_enabled)
    });
    patterns.sort_by(|a, b| {
        match query.sort.as_deref() {
            Some("name") => a.name.cmp(&b.name),
            Some("updated") => a.updated_at.cmp(&b.updated_at),
            _ => a.created_at.cmp(&b.created_at),
        }
        .then(a.id.cmp(&b.id))
    });
    if query.direction.as_deref() != Some("asc") {
        patterns.reverse();
    }
    let (offset, limit) = pagination.window()?;
    let has_next = patterns.len() > offset + limit;
    pagination.response(
        &uri,
        patterns
            .into_iter()
            .skip(offset)
            .take(limit)
            .map(|pattern| serde_json::to_value(pattern).map_err(Error::unavailable))
            .collect::<Result<Vec<_>>>()?,
        has_next,
    )
}
#[derive(Deserialize)]
struct CreateMany {
    patterns: Vec<Create>,
}
async fn create(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<HashMap<String, String>>,
    Json(input): Json<CreateMany>,
) -> Result<Response> {
    let (owner, actor) = scope(&state, &headers, &params, true).await?;
    let patterns = codeyard_db_security::secret_scanning::create_patterns(
        &state.pool,
        actor.user.id,
        &actor.credential(),
        owner,
        &input.patterns,
    )
    .await
    .map_err(error)?;
    Ok((
        StatusCode::CREATED,
        Json(json!({"created_patterns":patterns})),
    )
        .into_response())
}
async fn update(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<HashMap<String, String>>,
    Json(input): Json<Update>,
) -> Result<Json<Value>> {
    let (owner, actor) = scope(&state, &headers, &params, true).await?;
    let id = params["pattern_id"].parse().map_err(|_| Error::missing())?;
    let pattern = codeyard_db_security::secret_scanning::update_pattern(
        &state.pool,
        actor.user.id,
        &actor.credential(),
        owner,
        id,
        &input,
    )
    .await
    .map_err(error)?;
    Ok(Json(
        serde_json::to_value(pattern).map_err(Error::unavailable)?,
    ))
}
#[derive(Deserialize)]
struct DeleteMany {
    patterns: Vec<Delete>,
    post_delete_action: Option<String>,
}
async fn remove(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<HashMap<String, String>>,
    Json(input): Json<DeleteMany>,
) -> Result<StatusCode> {
    let (owner, actor) = scope(&state, &headers, &params, true).await?;
    if input
        .post_delete_action
        .as_deref()
        .is_some_and(|action| !matches!(action, "delete_alerts" | "resolve_alerts"))
    {
        return Err(Error::invalid("Invalid post_delete_action"));
    }
    codeyard_db_security::secret_scanning::delete_patterns(
        &state.pool,
        actor.user.id,
        &actor.credential(),
        owner,
        &input.patterns,
        input.post_delete_action.as_deref() == Some("resolve_alerts"),
    )
    .await
    .map_err(error)?;
    Ok(StatusCode::NO_CONTENT)
}
async fn configs(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<HashMap<String, String>>,
) -> Result<Json<Value>> {
    let (Owner::Organization(org), actor) = scope(&state, &headers, &params, false).await? else {
        return Err(Error::missing());
    };
    Ok(Json(
        codeyard_db_security::secret_scanning::pattern_configs(&state.pool, actor.user.id, org)
            .await
            .map_err(error)?,
    ))
}
async fn configure(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<HashMap<String, String>>,
    Json(input): Json<codeyard_db_security::secret_scanning::ConfigUpdate>,
) -> Result<Json<Value>> {
    let (Owner::Organization(org), actor) = scope(&state, &headers, &params, true).await? else {
        return Err(Error::missing());
    };
    let version = codeyard_db_security::secret_scanning::configure_patterns(
        &state.pool,
        actor.user.id,
        &actor.credential(),
        org,
        &input,
    )
    .await
    .map_err(error)?;
    Ok(Json(json!({"pattern_config_version":version})))
}
