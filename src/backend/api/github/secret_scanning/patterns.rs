use super::{auth, error as alert_error, repository, Error, Result};
use crate::backend::{
    app::AppState,
    db::secret_scanning as records,
    repositories::secret_scanning::patterns::{Create, Delete, Owner, Update},
};
use axum::{
    extract::{OriginalUri, Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{delete, get, patch, post, MethodRouter},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{collections::HashMap, sync::Arc};

fn error(error: anyhow::Error) -> Error {
    if matches!(
        error.downcast_ref::<records::Refusal>(),
        Some(records::Refusal::Invalid)
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
) -> Result<(Owner, auth::Authentication)> {
    if let Some(org) = params.get("org") {
        let actor = auth::authenticate(state, headers, true)
            .await?
            .ok_or_else(auth::required)?;
        if write {
            actor.require_write()?;
        }
        let org = super::super::orgs::target(state, org).await?;
        Ok((Owner::Organization(org.id), actor))
    } else {
        let (repo, actor) =
            repository(state, headers, &params["owner"], &params["repo"], write).await?;
        Ok((
            Owner::Repository(repo.id),
            actor.ok_or_else(auth::required)?,
        ))
    }
}
#[derive(Deserialize)]
struct Filters {
    state: Option<String>,
    push_protection: Option<String>,
    sort: Option<String>,
    direction: Option<String>,
    #[serde(flatten)]
    pagination: super::super::pagination::CursorPagination,
}
async fn list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<HashMap<String, String>>,
    OriginalUri(uri): OriginalUri,
    Query(query): Query<Filters>,
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
    let mut patterns = records::list_patterns(&state.pool, actor.user.id, owner)
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
    let pagination = &query.pagination;
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
    let patterns = records::create_patterns(&state.pool, actor.user.id, &actor.credential(), owner, &input.patterns)
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
    let pattern = records::update_pattern(&state.pool, actor.user.id, &actor.credential(), owner, id, &input)
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
    records::delete_patterns(
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
        records::pattern_configs(&state.pool, actor.user.id, org)
            .await
            .map_err(error)?,
    ))
}
async fn configure(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<HashMap<String, String>>,
    Json(input): Json<records::ConfigUpdate>,
) -> Result<Json<Value>> {
    let (Owner::Organization(org), actor) = scope(&state, &headers, &params, true).await? else {
        return Err(Error::missing());
    };
    let version = records::configure_patterns(&state.pool, actor.user.id, &actor.credential(), org, &input)
        .await
        .map_err(error)?;
    Ok(Json(json!({"pattern_config_version":version})))
}
