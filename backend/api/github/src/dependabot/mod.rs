//! Organization Dependabot credentials and repository access settings.
use crate::actions::action_secrets::error;
use crate::actions::action_secrets::open_value;
use axum::extract::rejection::JsonRejection;
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
use axum::routing::put;
use axum::routing::MethodRouter;
use axum::Json;
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use codeyard_api_http::shared::github::pagination::Pagination;
use codeyard_app::app::AppState;
use codeyard_db_security::dependabot::OrganizationSecret;
use codeyard_db_security::dependabot::OrganizationSecretInput;
use codeyard_db_security::dependabot::RepositorySelection;
use codeyard_types::github_error::Error;
use codeyard_types::github_error::Result;
use codeyard_types::urls::api_url;
use serde::Deserialize;
use serde_json::json;
use serde_json::Value;
use std::sync::Arc;

pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        (
            "dependabot/repository-access-for-org",
            get(repository_access),
        ),
        (
            "dependabot/update-repository-access-for-org",
            patch(update_repository_access),
        ),
        (
            "dependabot/set-repository-access-default-level",
            put(set_default_level),
        ),
        ("dependabot/list-org-secrets", get(list)),
        ("dependabot/get-org-public-key", get(public_key)),
        ("dependabot/get-org-secret", get(secret)),
        ("dependabot/create-or-update-org-secret", put(save)),
        ("dependabot/delete-org-secret", delete(remove)),
        (
            "dependabot/list-selected-repos-for-org-secret",
            get(selected),
        ),
        (
            "dependabot/set-selected-repos-for-org-secret",
            put(set_selected),
        ),
        (
            "dependabot/add-selected-repo-to-org-secret",
            put(add_selected),
        ),
        (
            "dependabot/remove-selected-repo-from-org-secret",
            delete(remove_selected),
        ),
    ]
}

pub(super) async fn organization(
    state: &AppState,
    headers: &HeaderMap,
    name: &str,
    write: bool,
) -> Result<(i64, codeyard_identity::github_auth::Authentication)> {
    let actor = codeyard_identity::github_auth::authenticate(state, headers, write)
        .await?
        .ok_or_else(codeyard_identity::github_auth::required)?;
    if write {
        actor.require_write()?;
    }
    let org = crate::orgs::target(state, name).await?;
    Ok((org.id, actor))
}

fn document(org: &str, secret: OrganizationSecret) -> Value {
    let mut value = json!(secret);
    if secret.visibility == "selected" {
        value["selected_repositories_url"] = json!(api_url(&format!(
            "/orgs/{}/dependabot/secrets/{}/repositories",
            codeyard_types::paths::encode_component(org),
            secret.name
        )));
    }
    value
}

async fn list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(name): Path<String>,
    OriginalUri(uri): OriginalUri,
    Query(page): Query<Pagination>,
) -> Result<Response> {
    let (org, actor) = organization(&state, &headers, &name, false).await?;
    let secrets = codeyard_db_security::dependabot::list_organization_secrets(
        &state.pool,
        actor.database_actor(),
        org,
    )
    .await
    .map_err(error)?;
    let (offset, limit) = page.window()?;
    let total = secrets.len();
    let secrets: Vec<_> = secrets
        .into_iter()
        .skip(offset)
        .take(limit)
        .map(|s| document(&name, s))
        .collect();
    Ok((
        page.headers(&uri, offset.saturating_add(limit) < total)?,
        Json(json!({"total_count":total,"secrets":secrets})),
    )
        .into_response())
}

pub(super) async fn public_key(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Result<Json<Value>> {
    let (org, actor) = organization(&state, &headers, &name, false).await?;
    let encryptor = state
        .secret_encryptor
        .as_deref()
        .ok_or_else(|| Error::unavailable(()))?;
    let (id, key) = codeyard_db_security::dependabot::organization_key(
        &state.pool,
        encryptor,
        actor.database_actor(),
        org,
    )
    .await
    .map_err(error)?;
    Ok(Json(
        json!({"key_id":id,"key":STANDARD.encode(key.public_key().as_bytes())}),
    ))
}

async fn secret(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((org_name, name)): Path<(String, String)>,
) -> Result<Json<Value>> {
    let (org, actor) = organization(&state, &headers, &org_name, false).await?;
    let secret = codeyard_db_security::dependabot::list_organization_secrets(
        &state.pool,
        actor.database_actor(),
        org,
    )
    .await
    .map_err(error)?
    .into_iter()
    .find(|s| s.name == name.to_ascii_uppercase())
    .ok_or_else(Error::missing)?;
    Ok(Json(document(&org_name, secret)))
}

#[derive(Deserialize)]
#[serde(untagged)]
enum RepositoryId {
    Number(i64),
    Text(String),
}
impl RepositoryId {
    fn value(self) -> Result<i64> {
        let id = match self {
            Self::Number(id) => Some(id),
            Self::Text(id) => id.parse().ok(),
        };
        id.filter(|id| *id > 0)
            .ok_or_else(|| Error::validation("Secret", "selected_repository_ids", "invalid"))
    }
}

#[derive(Deserialize)]
struct Save {
    key_id: Option<String>,
    encrypted_value: Option<String>,
    visibility: String,
    selected_repository_ids: Option<Vec<RepositoryId>>,
}

async fn save(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((org_name, name)): Path<(String, String)>,
    body: std::result::Result<Json<Save>, JsonRejection>,
) -> Result<Response> {
    let (org, actor) = organization(&state, &headers, &org_name, true).await?;
    let Json(body) = body.map_err(|_| Error::invalid("Invalid secret input"))?;
    let encryptor = state
        .secret_encryptor
        .as_deref()
        .ok_or_else(|| Error::unavailable(()))?;
    // The key operation checks live organization authority even for metadata-only updates.
    let (id, key) = codeyard_db_security::dependabot::organization_key(
        &state.pool,
        encryptor,
        actor.database_actor(),
        org,
    )
    .await
    .map_err(error)?;
    let value = match (body.encrypted_value, body.key_id) {
        (Some(value), Some(key_id)) => Some(open_value(&key, &id, &key_id, &value)?),
        (None, None) => None,
        _ => return Err(Error::validation("Secret", "key_id", "invalid")),
    };
    let ids = body
        .selected_repository_ids
        .map(|ids| {
            ids.into_iter()
                .map(RepositoryId::value)
                .collect::<Result<Vec<_>>>()
        })
        .transpose()?;
    let created = codeyard_db_security::dependabot::save_organization_secret(
        &state.pool,
        encryptor,
        actor.database_actor(),
        org,
        &OrganizationSecretInput {
            name: &name.to_ascii_uppercase(),
            value: value.as_deref(),
            visibility: &body.visibility,
            repositories: ids.as_deref(),
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

async fn remove(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((org_name, name)): Path<(String, String)>,
) -> Result<StatusCode> {
    let (org, actor) = organization(&state, &headers, &org_name, true).await?;
    if !codeyard_db_security::dependabot::delete_organization_secret(
        &state.pool,
        actor.database_actor(),
        org,
        &name.to_ascii_uppercase(),
    )
    .await
    .map_err(error)?
    {
        return Err(Error::missing());
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn selected(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((org_name, name)): Path<(String, String)>,
    OriginalUri(uri): OriginalUri,
    Query(page): Query<Pagination>,
) -> Result<Response> {
    let (org, actor) = organization(&state, &headers, &org_name, false).await?;
    let repositories = codeyard_db_security::dependabot::selected_repositories(
        &state.pool,
        actor.database_actor(),
        org,
        &name.to_ascii_uppercase(),
    )
    .await
    .map_err(error)?;
    let total = repositories.len();
    let (offset, limit) = page.window()?;
    let mut documents = Vec::new();
    for repo in repositories.into_iter().skip(offset).take(limit) {
        documents.push(
            codeyard_app::api_shared::github::repos::document(&state, &repo, Some(&actor)).await?,
        );
    }
    Ok((
        page.headers(&uri, offset.saturating_add(limit) < total)?,
        Json(json!({"total_count":total,"repositories":documents})),
    )
        .into_response())
}

#[derive(Deserialize)]
struct Selected {
    selected_repository_ids: Vec<RepositoryId>,
}

async fn set_selected(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((org_name, name)): Path<(String, String)>,
    body: std::result::Result<Json<Selected>, JsonRejection>,
) -> Result<StatusCode> {
    let (org, actor) = organization(&state, &headers, &org_name, true).await?;
    let Json(body) = body.map_err(|_| Error::invalid("Invalid repository selection"))?;
    let ids = body
        .selected_repository_ids
        .into_iter()
        .map(RepositoryId::value)
        .collect::<Result<Vec<_>>>()?;
    codeyard_db_security::dependabot::select_repositories(
        &state.pool,
        actor.database_actor(),
        org,
        &name.to_ascii_uppercase(),
        RepositorySelection::Set(&ids),
    )
    .await
    .map_err(error)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn repository_access(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(name): Path<String>,
    OriginalUri(uri): OriginalUri,
    Query(page): Query<Pagination>,
) -> Result<Response> {
    let (org, actor) = organization(&state, &headers, &name, false).await?;
    let (offset, limit) = page.window()?;
    let (level, mut repositories) = codeyard_db_security::dependabot::repository_access(
        &state.pool,
        actor.database_actor(),
        org,
        (offset as i64, limit as i64 + 1),
    )
    .await
    .map_err(error)?;
    let has_more = repositories.len() > limit;
    repositories.truncate(limit);
    let mut documents = Vec::new();
    for repository in repositories {
        documents.push(
            codeyard_app::api_shared::github::repos::document(&state, &repository, Some(&actor))
                .await?,
        );
    }
    Ok((
        page.headers(&uri, has_more)?,
        Json(json!({"default_level":level, "accessible_repositories":documents})),
    )
        .into_response())
}

#[derive(Deserialize)]
struct AccessUpdate {
    #[serde(default)]
    repository_ids_to_add: Vec<i64>,
    #[serde(default)]
    repository_ids_to_remove: Vec<i64>,
}

async fn update_repository_access(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(name): Path<String>,
    body: std::result::Result<Json<AccessUpdate>, JsonRejection>,
) -> Result<StatusCode> {
    let (org, actor) = organization(&state, &headers, &name, true).await?;
    let Json(body) = body.map_err(|_| Error::invalid("Invalid repository selection"))?;
    codeyard_db_security::dependabot::update_repository_access(
        &state.pool,
        actor.database_actor(),
        org,
        &body.repository_ids_to_add,
        &body.repository_ids_to_remove,
    )
    .await
    .map_err(error)?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct DefaultLevel {
    default_level: String,
}

async fn set_default_level(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(name): Path<String>,
    body: std::result::Result<Json<DefaultLevel>, JsonRejection>,
) -> Result<StatusCode> {
    let (org, actor) = organization(&state, &headers, &name, true).await?;
    let Json(body) =
        body.map_err(|_| Error::validation("Dependabot", "default_level", "invalid"))?;
    if !matches!(body.default_level.as_str(), "public" | "internal") {
        return Err(Error::validation("Dependabot", "default_level", "invalid"));
    }
    codeyard_db_security::dependabot::set_repository_access_default(
        &state.pool,
        actor.database_actor(),
        org,
        &body.default_level,
    )
    .await
    .map_err(error)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn add_selected(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((org_name, name, id)): Path<(String, String, i64)>,
) -> Result<StatusCode> {
    let (org, actor) = organization(&state, &headers, &org_name, true).await?;
    codeyard_db_security::dependabot::select_repositories(
        &state.pool,
        actor.database_actor(),
        org,
        &name.to_ascii_uppercase(),
        RepositorySelection::Add(id),
    )
    .await
    .map_err(error)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn remove_selected(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((org_name, name, id)): Path<(String, String, i64)>,
) -> Result<StatusCode> {
    let (org, actor) = organization(&state, &headers, &org_name, true).await?;
    codeyard_db_security::dependabot::select_repositories(
        &state.pool,
        actor.database_actor(),
        org,
        &name.to_ascii_uppercase(),
        RepositorySelection::Remove(id),
    )
    .await
    .map_err(error)?;
    Ok(StatusCode::NO_CONTENT)
}

pub mod dependabot_alerts;
