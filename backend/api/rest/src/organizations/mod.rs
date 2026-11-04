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

pub fn router() -> axum::Router<std::sync::Arc<codeyard_app::app::AppState>> {
    use axum::{
        extract::DefaultBodyLimit,
        routing::{get, post},
        Router,
    };
    Router::new()
        .route(
            INDEX,
            get(crate::organizations::collection::show)
                .post(crate::organizations::organization::mutate),
        )
        .route(
            crate::organizations::governance::RECOVERY,
            post(crate::organizations::governance::recover),
        )
        .route(
            "/settings/organizations/invitations/:org/accept",
            post(crate::organizations::invitations::accept),
        )
        .route(
            "/settings/organizations/invitations/:org/decline",
            post(crate::organizations::invitations::decline),
        )
        .route(
            "/orgs/:org/teams/:team",
            get(crate::organizations::teams::public_show),
        )
        .route(
            "/settings/organizations/:id/dependabot",
            post(crate::organizations::dependabot::post),
        )
        .route(
            "/settings/organizations/:id/webhooks/:action",
            post(crate::organizations::webhooks::post),
        )
        .route(
            "/settings/organizations/:id",
            get(crate::organizations::organization::show)
                .post(crate::organizations::organization::mutate),
        )
        .merge(crate::organizations::settings::router())
        .merge(crate::organizations::tokens::router())
        .layer(DefaultBodyLimit::max(
            crate::organizations::organization::BODY_LIMIT,
        ))
        .route(
            "/settings/organizations/:id/private-registries",
            post(crate::organizations::private_registries::post).layer(DefaultBodyLimit::max(
                crate::organizations::private_registries::BODY_LIMIT,
            )),
        )
        .merge(crate::organizations::properties::router())
        .merge(crate::organizations::avatar::router())
        .merge(crate::organizations::action_values::router())
        .merge(crate::organizations::code_security::router())
}
