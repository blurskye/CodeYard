//! Repository settings: who can read and write, collaborators, secrets and branch protection.
use axum::routing::delete;
use axum::routing::get;
use axum::routing::put;
use axum::Router;
use codeyard_api_http::cell_routes::CellRoutes;
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
        .cell_route(
            "/api/repos/:owner/:repo/permissions",
            get(crate::settings::permissions::get_repo_permissions),
        )
        .cell_route(
            "/api/repos/:owner/:repo/collaborators",
            get(crate::settings::collaborators::list_collaborators)
                .post(crate::settings::collaborators::add_collaborator),
        )
        .cell_route(
            "/api/repos/:owner/:repo/collaborators/:username",
            delete(crate::settings::collaborators::remove_collaborator),
        )
        .cell_route(
            "/api/repos/:owner/:repo/secrets",
            get(crate::settings::secrets::list_secrets),
        )
        .cell_route(
            "/api/repos/:owner/:repo/secrets/:name",
            put(crate::settings::secrets::create_or_update_secret)
                .delete(crate::settings::secrets::delete_secret),
        )
        .merge(crate::settings::branch_protection::router())
}
