//! Repository settings: who can read and write, collaborators, secrets and branch protection.
use axum::routing::delete;
use axum::routing::get;
use axum::routing::put;
use axum::Router;
use codeyard_app::app::AppState;
use std::sync::Arc;

pub mod branch_protection;
pub mod collaborators;
pub(super) mod environment_values;
pub mod permissions;
pub mod secrets;
pub(super) mod variables;

pub fn json_router() -> Router<Arc<AppState>> {
    Router::new()
        .route(
            "/api/repos/:owner/:repo/permissions",
            get(crate::settings::permissions::get_repo_permissions),
        )
        .route(
            "/api/repos/:owner/:repo/collaborators",
            get(crate::settings::collaborators::list_collaborators)
                .post(crate::settings::collaborators::add_collaborator),
        )
        .route(
            "/api/repos/:owner/:repo/collaborators/:username",
            delete(crate::settings::collaborators::remove_collaborator),
        )
        .route(
            "/api/repos/:owner/:repo/secrets",
            get(crate::settings::secrets::list_secrets),
        )
        .route(
            "/api/repos/:owner/:repo/secrets/:name",
            put(crate::settings::secrets::create_or_update_secret)
                .delete(crate::settings::secrets::delete_secret),
        )
        .merge(crate::settings::branch_protection::router())
}
