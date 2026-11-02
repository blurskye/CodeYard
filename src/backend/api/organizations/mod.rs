//! Organization browser resources; authorization remains in named DB transactions.

mod action_values;
mod avatar;
mod code_security;
mod collection;
mod dependabot;
mod governance;
mod invitations;
mod membership;
mod organization;
mod private_registries;
pub(crate) mod profile;
mod properties;
mod roles;
mod settings;
mod teams;
mod tokens;
mod webhooks;

const INDEX: &str = "/settings/organizations";

pub fn router() -> axum::Router<std::sync::Arc<crate::backend::app::AppState>> {
    use axum::{
        extract::DefaultBodyLimit,
        routing::{get, post},
        Router,
    };
    Router::new()
        .route(INDEX, get(collection::show).post(organization::mutate))
        .route(governance::RECOVERY, post(governance::recover))
        .route(
            "/settings/organizations/invitations/:org/accept",
            post(invitations::accept),
        )
        .route(
            "/settings/organizations/invitations/:org/decline",
            post(invitations::decline),
        )
        .route("/orgs/:org/teams/:team", get(teams::public_show))
        .route(
            "/settings/organizations/:id/dependabot",
            post(dependabot::post),
        )
        .route(
            "/settings/organizations/:id/private-registries",
            post(private_registries::post),
        )
        .route(
            "/settings/organizations/:id/webhooks/:action",
            post(webhooks::post),
        )
        .route(
            "/settings/organizations/:id",
            get(organization::show).post(organization::mutate),
        )
        .merge(settings::router())
        .merge(tokens::router())
        .layer(DefaultBodyLimit::max(organization::BODY_LIMIT))
        .merge(properties::router())
        .merge(avatar::router())
        .merge(action_values::router())
        .merge(code_security::router())
}
