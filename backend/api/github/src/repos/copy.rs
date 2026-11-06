//! Forks and template generation use the same authoritative creation and storage pipeline as forms.

use crate::optional_json::OptionalJson;
use axum::extract::Path;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::routing::post;
use axum::routing::MethodRouter;
use axum::Json;
use codeyard_app::api_shared::github::repos::document;
use codeyard_app::api_shared::github::repos::lifecycle_error;
use codeyard_app::api_shared::github::repository;
use codeyard_app::app::AppState;
use codeyard_db_core::models::User;
use codeyard_db_repos::repositories::creation::RepositoryCreationOwner;
use codeyard_db_repos::repositories::creation::RepositoryTarget;
use codeyard_model::github_error::Error;
use codeyard_model::github_error::Result;
use serde::Deserialize;
use serde_json::Value;
use std::sync::Arc;

async fn organization_owner(
    state: &AppState,
    actor: i64,
    slug: &str,
) -> Result<RepositoryCreationOwner> {
    let org = codeyard_db_orgs::organizations::profiles::get_public_organization(&state.pool, slug)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(Error::missing)?;
    Ok(RepositoryCreationOwner {
        actor_id: actor,
        target: RepositoryTarget::Organization(org.id),
    })
}
async fn owner(
    state: &AppState,
    actor: &User,
    name: Option<&str>,
) -> Result<RepositoryCreationOwner> {
    match name {
        Some(name) if name != actor.username => organization_owner(state, actor.id, name).await,
        _ => Ok(RepositoryCreationOwner::personal(actor.id)),
    }
}

#[derive(Default, Deserialize)]
struct Fork {
    organization: Option<String>,
    name: Option<String>,
    #[serde(default)]
    default_branch_only: bool,
}
async fn fork(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
    OptionalJson(body): OptionalJson<Value>,
) -> Result<(StatusCode, Json<Value>)> {
    let (source, actor) = repository(&state, &headers, &owner, &name, true).await?;
    let auth = actor.ok_or_else(codeyard_identity::github_auth::required)?;
    let actor = &auth.user;
    let input = body
        .map(serde_json::from_value::<Option<Fork>>)
        .transpose()
        .map_err(|_| Error::invalid("Invalid fork fields"))?
        .flatten()
        .unwrap_or_default();
    let target = match input.organization.as_deref() {
        Some(slug) => organization_owner(&state, actor.id, slug).await?,
        None => RepositoryCreationOwner::personal(actor.id),
    };
    crate::auth::require_copy_owner(&state, &auth, target.target).await?;
    let repo = codeyard_repos_core::repositories::lifecycle::fork_into(
        &state.pool,
        state.cluster.as_ref(),
        &state.repos_path,
        &state.notification_broadcaster,
        codeyard_repos_core::repositories::lifecycle::ForkOptions {
            foreign_source: None,
            owner: target,
            name: input.name.as_deref(),
            default_branch_only: input.default_branch_only,
            properties: &[],
            authority: Some(&auth.database_actor()),
        },
        source.id,
        &owner,
    )
    .await
    .map_err(lifecycle_error)?;
    Ok((
        StatusCode::ACCEPTED,
        Json(document(&state, &repo, Some(&auth)).await?),
    ))
}

#[derive(Deserialize)]
struct Generate {
    owner: Option<String>,
    name: String,
    description: Option<String>,
    #[serde(default)]
    private: bool,
    #[serde(default)]
    include_all_branches: bool,
}
async fn generate(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((template_owner, template_name)): Path<(String, String)>,
    Json(input): Json<Generate>,
) -> Result<(StatusCode, Json<Value>)> {
    let (source, actor) =
        repository(&state, &headers, &template_owner, &template_name, true).await?;
    let auth = actor.ok_or_else(codeyard_identity::github_auth::required)?;
    let target = owner(&state, &auth.user, input.owner.as_deref()).await?;
    crate::auth::require_copy_owner(&state, &auth, target.target).await?;
    let repo = codeyard_repos_core::repositories::lifecycle::generate(
        (&state.pool, state.cluster.as_ref()),
        &state.repos_path,
        target,
        source.id,
        codeyard_repos_core::repositories::lifecycle::TemplateOptions {
            name: &input.name,
            description: input.description.as_deref(),
            private: input.private,
            all_branches: input.include_all_branches,
            properties: &[],
            authority: Some(&auth.database_actor()),
        },
    )
    .await
    .map_err(lifecycle_error)?;
    Ok((
        StatusCode::CREATED,
        Json(document(&state, &repo, Some(&auth)).await?),
    ))
}

pub(crate) fn handlers() -> [(&'static str, MethodRouter<Arc<AppState>>); 2] {
    [
        ("repos/create-fork", post(fork)),
        ("repos/create-using-template", post(generate)),
    ]
}
