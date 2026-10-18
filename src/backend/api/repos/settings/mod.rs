//! Repository settings: who can read and write, collaborators, secrets and branch protection.
use crate::backend::app::AppState;
use axum::{
    routing::{delete, get, put},
    Router,
};
use std::sync::Arc;

pub mod branch_protection;
pub mod collaborators;
pub mod permissions;
pub mod secrets;

pub fn json_router() -> Router<Arc<AppState>> {
    Router::new()
        .route(
            "/api/repos/:owner/:repo/permissions",
            get(permissions::get_repo_permissions),
        )
        .route(
            "/api/repos/:owner/:repo/collaborators",
            get(collaborators::list_collaborators).post(collaborators::add_collaborator),
        )
        .route(
            "/api/repos/:owner/:repo/collaborators/:username",
            delete(collaborators::remove_collaborator),
        )
        .route(
            "/api/repos/:owner/:repo/secrets",
            get(secrets::list_secrets),
        )
        .route(
            "/api/repos/:owner/:repo/secrets/:name",
            put(secrets::create_or_update_secret).delete(secrets::delete_secret),
        )
        .merge(branch_protection::router())
}
