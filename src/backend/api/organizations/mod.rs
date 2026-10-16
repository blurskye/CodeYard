//! Organization browser resources; authorization remains in named DB transactions.

mod avatar;
mod collection;
mod governance;
mod membership;
mod organization;
pub(crate) mod profile;
mod teams;

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
        .route("/orgs/:org/teams/:team", get(teams::public_show))
        .route(
            "/settings/organizations/:id",
            get(organization::show).post(organization::mutate),
        )
        .layer(DefaultBodyLimit::max(organization::BODY_LIMIT))
        .merge(avatar::router())
}
