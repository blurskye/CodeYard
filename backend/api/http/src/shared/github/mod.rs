use axum::http::HeaderMap;
use codeyard_app::app::AppState;
pub(crate) const RELEASE_ASSET_UPLOAD_ROUTE: &str =
    "/api/v3/repos/:owner/:repo/releases/:release_id/assets";

/// Gist writes read their own larger bodies (see `crate::shared::github::gists::buffered_write`).
pub(crate) const GISTS_ROUTE: &str = "/api/v3/gists";

pub(crate) const GIST_ROUTE: &str = "/api/v3/gists/:gist_id";

/// Non-authoritative identity lookup for derived Git proof preparation. The actual handler
/// still owns authentication, token scope, rate limits and the final domain authority check.
pub(crate) async fn interaction_actor(
    state: &AppState,
    headers: &HeaderMap,
) -> Option<codeyard_database::models::User> {
    let credential = codeyard_identity::github_auth::credential(headers)?;
    let token = codeyard_db_identity::users::tokens::find_token(
        &state.pool,
        &codeyard_support::token_hash::token_hash(credential),
    )
    .await
    .ok()??;
    codeyard_db_identity::users::tokens::token_user(&state.pool, token.id, false)
        .await
        .ok()?
}

pub mod code_scanning;

pub mod gists;

pub mod git_objects;

pub mod pagination;
