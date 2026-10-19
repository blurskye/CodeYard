//! Code: browsing the tree and files, raw and archive downloads, editing and uploading file
//! content, and the language and contributor summaries.
use crate::backend::app::AppState;
use axum::{
    extract::DefaultBodyLimit,
    routing::{get, put},
    Router,
};
use std::sync::Arc;

pub mod archive;
pub mod blame;
pub mod blob;
pub mod contents;
pub mod contributors;
pub mod find;
pub mod languages;
pub mod raw;
pub mod tree;
pub mod upload;

pub fn json_router() -> Router<Arc<AppState>> {
    Router::new()
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
            "/api/repos/:owner/:repo/contents/*path",
            put(contents::update_file_content)
                .post(contents::create_file)
                .delete(contents::delete_file_content)
                .layer(axum::middleware::from_fn(
                    crate::backend::api::http::middleware::private_sensitive_response,
                )),
        )
        .route(
            "/api/repos/:owner/:repo/languages",
            get(languages::get_repo_languages),
        )
        .route(
            "/api/repos/:owner/:repo/contributors",
            get(contributors::get_repo_contributors),
        )
}

/// Browser pages for files, the file editor and downloads. The caller applies the form body
/// limit; none of these GET pages reads a body.
pub fn page_router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/:owner/:repo", get(tree::page))
        .route("/:owner/:repo/tree/*ref_path", get(tree::page))
        .route("/:owner/:repo/blob/*ref_path", get(blob::page))
        .route("/:owner/:repo/blame/*ref_path", get(blame::page))
        .route("/:owner/:repo/find/*ref_path", get(find::page))
        .route("/:owner/:repo/new/*ref_path", get(contents::page))
        .route("/:owner/:repo/edit/*ref_path", get(contents::page))
        .route("/:owner/:repo/delete/*ref_path", get(contents::page))
}

/// Raw and archive downloads sit outside the form body limit, as they always have.
pub fn download_router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/:owner/:repo/raw/*ref_path", get(raw::raw_page))
        .route("/:owner/:repo/archive/*archive", get(archive::page))
}

/// Uploads carry whole files, so this router has its own, larger body limit.
pub fn upload_router() -> Router<Arc<AppState>> {
    Router::new()
        .route(upload::ROUTE, get(upload::page).post(upload::post))
        .layer(DefaultBodyLimit::max(upload::BODY_LIMIT))
}
