//! Activity on a repository: stars, watchers, stargazers and forks.
use crate::backend::app::AppState;
use axum::{routing::get, Router};
use std::sync::Arc;

pub mod engagement;
pub mod forks;
pub mod stargazers;

pub fn json_router() -> Router<Arc<AppState>> {
    Router::new()
        .route(
            "/api/repos/:owner/:repo/star",
            get(engagement::check_star)
                .put(engagement::star_repo)
                .delete(engagement::unstar_repo),
        )
        .route(
            "/api/repos/:owner/:repo/watch",
            get(engagement::check_watch)
                .put(engagement::watch_repo)
                .delete(engagement::unwatch_repo),
        )
        .route(
            "/api/repos/:owner/:repo/stargazers",
            get(stargazers::list_stargazers),
        )
        .route(
            "/api/repos/:owner/:repo/forks",
            get(forks::list_forks).post(forks::fork_repo),
        )
        .route("/api/repos/:owner/:repo/fork", get(forks::check_fork))
}
