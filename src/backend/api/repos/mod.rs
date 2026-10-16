use crate::backend::app::AppState;
use axum::{
    routing::{delete, get, post, put},
    Router,
};
use std::sync::Arc;
pub mod access;
pub mod archive;
pub mod blame;
pub mod blob;
pub mod branch_protection;
pub mod branches;
pub mod chrome;
pub mod collaborators;
pub mod collection;
pub mod commits;
pub mod compare;
pub mod contents;
pub mod contributors;
pub mod find;
pub mod forks;
pub mod issues;
pub mod labels;
pub mod languages;
pub mod milestones;
pub mod permissions;
pub mod pulls;
pub mod raw;
pub mod references;
pub mod repository;
pub mod responses;
pub mod secrets;
pub mod star;
pub mod stargazers;
pub mod tags;
pub mod tree;
pub mod upload;
pub mod watch;

pub fn router() -> Router<Arc<AppState>> {
    let json = Router::new()
        .route(
            "/api/repos",
            get(collection::list_repos).post(collection::create_repo),
        )
        .route(
            "/api/repos/:owner/:repo",
            get(repository::get_repo)
                .patch(repository::update_repo)
                .delete(repository::delete_repo),
        )
        .route(
            "/api/repos/:owner/:repo/permissions",
            get(permissions::get_repo_permissions),
        )
        .route(
            "/api/repos/:owner/:repo/rename",
            post(repository::rename_repo),
        )
        .route(
            "/api/repos/:owner/:repo/transfer",
            post(repository::transfer_repo),
        )
        .route(
            "/api/repos/:owner/:repo/tree/:ref",
            get(tree::get_tree_root),
        )
        .route(
            "/api/repos/:owner/:repo/tree/:ref/*path",
            get(tree::get_tree),
        )
        .route(
            "/api/repos/:owner/:repo/blob/:ref/*path",
            get(blob::get_blob),
        )
        .route(
            "/api/repos/:owner/:repo/raw/:ref/*path",
            get(raw::get_raw_blob),
        )
        .route(
            "/api/repos/:owner/:repo/archive",
            get(archive::download_archive),
        )
        .route(
            "/api/repos/:owner/:repo/branches",
            get(branches::list_branches).post(branches::create_branch),
        )
        .route(
            "/api/repos/:owner/:repo/branches/:branch",
            delete(branches::delete_branch),
        )
        .route(
            "/api/repos/:owner/:repo/tags",
            get(tags::list_tags).post(tags::create_tag),
        )
        .route(
            "/api/repos/:owner/:repo/tags/:name",
            delete(tags::delete_tag),
        )
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
            "/api/repos/:owner/:repo/contents/*path",
            put(contents::update_file_content)
                .post(contents::create_file)
                .delete(contents::delete_file_content)
                .layer(axum::middleware::from_fn(
                    crate::backend::api::middleware::private_sensitive_response,
                )),
        )
        .route(
            "/api/repos/:owner/:repo/forks",
            get(forks::list_forks).post(forks::fork_repo),
        )
        .route("/api/repos/:owner/:repo/fork", get(forks::check_fork))
        .route(
            "/api/repos/:owner/:repo/collaborators",
            get(collaborators::list_collaborators).post(collaborators::add_collaborator),
        )
        .route(
            "/api/repos/:owner/:repo/collaborators/:username",
            delete(collaborators::remove_collaborator),
        )
        .route(
            "/api/repos/:owner/:repo/languages",
            get(languages::get_repo_languages),
        )
        .route(
            "/api/repos/:owner/:repo/contributors",
            get(contributors::get_repo_contributors),
        )
        .route(
            "/api/repos/:owner/:repo/secrets",
            get(secrets::list_secrets),
        )
        .route(
            "/api/repos/:owner/:repo/secrets/:name",
            put(secrets::create_or_update_secret).delete(secrets::delete_secret),
        )
        .merge(labels::router())
        .merge(branch_protection::router())
        .merge(issues::json_router())
        .merge(pulls::json_router())
        .route_layer(axum::middleware::from_fn(
            crate::backend::api::middleware::require_node_access,
        ))
        .route_layer(axum::middleware::from_fn(
            crate::backend::api::csrf::cookie_api_submission,
        ))
        .layer(axum::middleware::from_fn(
            crate::backend::api::middleware::private_api_response,
        ));
    let creation = Router::new()
        .route(
            "/new",
            get(collection::new_repository).post(collection::create_repository),
        )
        .layer(axum::extract::DefaultBodyLimit::max(16 * 1024));
    let pages = Router::new()
        .route("/:owner/:repo", get(tree::page))
        .route("/:owner/:repo/tree/*ref_path", get(tree::page))
        .route("/:owner/:repo/blob/*ref_path", get(blob::page))
        .route("/:owner/:repo/blame/*ref_path", get(blame::page))
        .route("/:owner/:repo/find/*ref_path", get(find::page))
        .route("/:owner/:repo/branches", get(branches::page))
        .route("/:owner/:repo/compare", get(compare::native_compare_page))
        .route(
            "/:owner/:repo/compare/*spec",
            get(compare::native_compare_page),
        )
        .route("/:owner/:repo/tags", get(tags::page))
        .route("/:owner/:repo/new/*ref_path", get(contents::page))
        .route("/:owner/:repo/edit/*ref_path", get(contents::page))
        .route("/:owner/:repo/delete/*ref_path", get(contents::page))
        .route("/:owner/:repo/settings", get(repository::page))
        .route(
            "/:owner/:repo/_forms/:action",
            get(repository::get_form).post(repository::post_form),
        )
        .layer(axum::extract::DefaultBodyLimit::max(4 * 1024 * 1024));
    // Uploads carry whole files, so only this route accepts a body as large as its limit.
    let uploads = Router::new()
        .route(upload::ROUTE, get(upload::page).post(upload::post))
        .layer(axum::extract::DefaultBodyLimit::max(upload::BODY_LIMIT));
    Router::new()
        .merge(json)
        .merge(creation)
        .merge(pages)
        .merge(uploads)
        .merge(commits::router())
        .merge(issues::page_router())
        .merge(pulls::page_router())
        .merge(milestones::page_router())
        .route("/:owner/:repo/raw/*ref_path", get(raw::raw_page))
        .route("/:owner/:repo/archive", get(archive::page))
}
