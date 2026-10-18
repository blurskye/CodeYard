//! Activity on a repository: stars, watchers, stargazers and forks.
use crate::backend::app::AppState;
use axum::{routing::get, Router};
use std::sync::Arc;

pub mod forks;
pub mod star;
pub mod stargazers;
pub mod watch;

pub fn json_router() -> Router<Arc<AppState>> {
    Router::new()
        .route(
            "/api/repos/:owner/:repo/star",
            get(star::check_star)
                .put(star::star_repo)
                .delete(star::unstar_repo),
        )
        .route(
            "/api/repos/:owner/:repo/watch",
            get(watch::check_watch)
                .put(watch::watch_repo)
                .delete(watch::unwatch_repo),
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
