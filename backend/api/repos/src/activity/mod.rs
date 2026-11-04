//! Activity on a repository: stars, watchers, stargazers and forks.
use axum::routing::get;
use axum::Router;
use codeyard_app::app::AppState;
use std::sync::Arc;

pub mod engagement;
pub mod forks;
pub mod stargazers;

pub fn json_router() -> Router<Arc<AppState>> {
    Router::new()
        .route(
            "/api/repos/:owner/:repo/star",
            get(crate::activity::engagement::check_star)
                .put(crate::activity::engagement::star_repo)
                .delete(crate::activity::engagement::unstar_repo),
        )
        .route(
            "/api/repos/:owner/:repo/watch",
            get(crate::activity::engagement::check_watch)
                .put(crate::activity::engagement::watch_repo)
                .delete(crate::activity::engagement::unwatch_repo),
        )
        .route(
            "/api/repos/:owner/:repo/stargazers",
            get(crate::activity::stargazers::list_stargazers),
        )
        .route(
            "/api/repos/:owner/:repo/forks",
            get(crate::activity::forks::list_forks).post(crate::activity::forks::fork_repo),
        )
        .route(
            "/api/repos/:owner/:repo/fork",
            get(crate::activity::forks::check_fork),
        )
}
