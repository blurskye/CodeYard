//! Code: browsing the tree and files, raw and archive downloads, editing and uploading file
//! content, and the language and contributor summaries.
use axum::extract::DefaultBodyLimit;
use axum::routing::get;
use axum::routing::put;
use axum::Router;
use codeyard_app::app::AppState;
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

/// Path<String> rejects valid Git filenames before a handler runs. Decode the original URI
/// into reversible path identities instead; account/repository names must still be UTF-8.
pub struct GitPathParams(pub std::collections::HashMap<String, String>);

#[axum::async_trait]
impl<S: Send + Sync> axum::extract::FromRequestParts<S> for GitPathParams {
    type Rejection = axum::http::StatusCode;
    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _: &S,
    ) -> Result<Self, Self::Rejection> {
        let uri = parts
            .extensions
            .get::<axum::extract::OriginalUri>()
            .map(|uri| &uri.0)
            .unwrap_or(&parts.uri);
        // A legacy `gists` account's repositories are served under the alias prefix.
        let path = uri
            .path()
            .strip_prefix(codeyard_api_http::shared::repos::LEGACY_PREFIX)
            .filter(|rest| rest.starts_with('/'))
            .unwrap_or(uri.path());
        let mut pieces = path.trim_start_matches('/').splitn(4, '/');
        let bad = axum::http::StatusCode::BAD_REQUEST;
        let owner = codeyard_support::git_paths::from_url(pieces.next().ok_or(bad)?).ok_or(bad)?;
        let repo = codeyard_support::git_paths::from_url(pieces.next().ok_or(bad)?).ok_or(bad)?;
        if owner.contains('\0') || repo.contains('\0') {
            return Err(bad);
        }
        let mut params =
            std::collections::HashMap::from([("owner".into(), owner), ("repo".into(), repo)]);
        if pieces.next().is_some() {
            let tail =
                codeyard_support::git_paths::from_url(pieces.next().unwrap_or("")).ok_or(bad)?;
            params.insert("ref_path".into(), tail);
        }
        Ok(Self(params))
    }
}

pub fn json_router() -> Router<Arc<AppState>> {
    Router::new()
        .route(
            "/api/repos/:owner/:repo/tree/:ref",
            get(crate::code::tree::get_tree_root),
        )
        .route(
            "/api/repos/:owner/:repo/tree/:ref/*path",
            get(crate::code::tree::get_tree),
        )
        .route(
            "/api/repos/:owner/:repo/blob/:ref/*path",
            get(crate::code::blob::get_blob),
        )
        .route(
            "/api/repos/:owner/:repo/raw/:ref/*path",
            get(crate::code::raw::get_raw_blob),
        )
        .route(
            "/api/repos/:owner/:repo/archive",
            get(crate::code::archive::download_archive),
        )
        .route(
            "/api/repos/:owner/:repo/contents/*path",
            put(crate::code::contents::update_file_content)
                .post(crate::code::contents::create_file)
                .delete(crate::code::contents::delete_file_content)
                .layer(axum::middleware::from_fn(
                    codeyard_api_http::middleware::private_sensitive_response,
                )),
        )
        .route(
            "/api/repos/:owner/:repo/languages",
            get(crate::code::languages::get_repo_languages),
        )
        .route(
            "/api/repos/:owner/:repo/contributors",
            get(crate::code::contributors::get_repo_contributors),
        )
}

/// Browser pages for files, the file editor and downloads. The caller applies the form body
/// limit; none of these GET pages reads a body.
pub fn page_router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/:owner/:repo", get(crate::code::tree::page))
        .route("/:owner/:repo/tree/*ref_path", get(crate::code::tree::page))
        .route("/:owner/:repo/blob/*ref_path", get(crate::code::blob::page))
        .route(
            "/:owner/:repo/blame/*ref_path",
            get(crate::code::blame::page),
        )
        .route("/:owner/:repo/find/*ref_path", get(crate::code::find::page))
        .route(
            "/:owner/:repo/new/*ref_path",
            get(crate::code::contents::page),
        )
        .route(
            "/:owner/:repo/edit/*ref_path",
            get(crate::code::contents::page),
        )
        .route(
            "/:owner/:repo/delete/*ref_path",
            get(crate::code::contents::page),
        )
}

/// Raw and archive downloads sit outside the form body limit, as they always have.
pub fn download_router() -> Router<Arc<AppState>> {
    Router::new()
        .route(
            "/:owner/:repo/raw/*ref_path",
            get(crate::code::raw::raw_page),
        )
        .route(
            "/:owner/:repo/archive/*archive",
            get(crate::code::archive::page),
        )
}

/// Uploads carry whole files, so this router has its own, larger body limit.
pub fn upload_router() -> Router<Arc<AppState>> {
    Router::new()
        .route(
            codeyard_api_http::shared::repos::code::upload::ROUTE,
            get(crate::code::upload::page).post(crate::code::upload::post),
        )
        .layer(DefaultBodyLimit::max(crate::code::upload::BODY_LIMIT))
}
