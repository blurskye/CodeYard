//! Native gist pages and forms share the Git-backed domain with the GitHub API.
use axum::extract::DefaultBodyLimit;
use axum::extract::Form;
use axum::extract::Path;
use axum::extract::State;
use axum::http::header;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::http::Uri;
use axum::response::Response;
use axum::routing::get;
use axum::routing::post;
use axum::Router;
use codeyard_api_http::cell_routes::CellRoutes;
use codeyard_api_http::context::PageContext;
use codeyard_api_http::session;
use codeyard_app::app::AppState;
use codeyard_app::gists as domain;
use codeyard_git::error::ErrorKind;
use codeyard_git::error::RepoError;
use codeyard_git_storage::repositories::gists::GistCredential;
use codeyard_git_storage::repositories::lifecycle::LifecycleFailure;
use codeyard_git_storage::repositories::lifecycle::StorageError;
use codeyard_webui::app::Page as AppPage;
use codeyard_webui::pages::gists as view;
use std::sync::Arc;

mod forms;
mod pages;
const PAGE_SIZE: usize = 30;
const EDITOR_BYTES: usize = 1024 * 1024;
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct InvalidInput(&'static str);
pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .cell_route("/gists", get(crate::gists::pages::list))
        .cell_route(
            "/gists/new",
            get(crate::gists::pages::new).post(crate::gists::forms::create),
        )
        .cell_route("/gists/:id", get(crate::gists::pages::read))
        .cell_route(
            "/gists/:id/edit",
            get(crate::gists::pages::edit).post(crate::gists::forms::edit),
        )
        .cell_route(
            "/gists/:id/raw/:sha/:filename",
            get(crate::gists::pages::raw),
        )
        .cell_route("/gists/:id/star", post(crate::gists::forms::star))
        .cell_route("/gists/:id/fork", post(crate::gists::forms::fork))
        .cell_route("/gists/:id/delete", post(crate::gists::forms::delete))
        .cell_route("/gists/:id/comments", post(crate::gists::forms::comment))
        .cell_route(
            "/gists/:id/comments/:comment/edit",
            post(crate::gists::forms::edit_comment),
        )
        .cell_route(
            "/gists/:id/comments/:comment/delete",
            post(crate::gists::forms::delete_comment),
        )
        .layer(DefaultBodyLimit::max(4 * 1024 * 1024))
}
fn refusal(error: &anyhow::Error) -> (StatusCode, String) {
    if let Some(error) = error.downcast_ref::<InvalidInput>() {
        return (StatusCode::UNPROCESSABLE_ENTITY, error.to_string());
    }
    if let Some(error) = error.downcast_ref::<codeyard_db_repos::gists::GistRefusal>() {
        return (
            match error {
                codeyard_db_repos::gists::GistRefusal::Missing => StatusCode::NOT_FOUND,
                codeyard_db_repos::gists::GistRefusal::Denied => StatusCode::FORBIDDEN,
                codeyard_db_repos::gists::GistRefusal::Conflict => StatusCode::CONFLICT,
                codeyard_db_repos::gists::GistRefusal::Invalid => StatusCode::UNPROCESSABLE_ENTITY,
            },
            error.to_string(),
        );
    }
    if error.is::<codeyard_types::credentials::AccountRestricted>() {
        return (
            StatusCode::FORBIDDEN,
            "Your session no longer permits this change. Sign in again.".into(),
        );
    }
    if error.is::<codeyard_app::gists::GistSaved>()
        || error.is::<codeyard_app::gists::GistDeleted>()
    {
        return (StatusCode::SERVICE_UNAVAILABLE, error.to_string());
    }
    if let Some(error) = error.downcast_ref::<RepoError>() {
        if error.is_write_denied() {
            return (
                StatusCode::FORBIDDEN,
                "Only the gist owner may edit its files.".into(),
            );
        }
        if error.is_response_too_large() {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                "Gist content or history exceeds the supported limit.".into(),
            );
        }
        match error.kind {
            ErrorKind::NotFound | ErrorKind::MissingPath => {
                return (
                    StatusCode::NOT_FOUND,
                    "Gist revision or file not found.".into(),
                )
            }
            ErrorKind::Invalid => return (StatusCode::UNPROCESSABLE_ENTITY, error.message.clone()),
            ErrorKind::Conflict => {
                return (
                    StatusCode::CONFLICT,
                    "The gist changed. Your draft is kept below; reload before retrying.".into(),
                )
            }
            ErrorKind::Unavailable => {}
        }
    }
    if let Some(error) = error.downcast_ref::<StorageError>() {
        match error.kind {
            LifecycleFailure::OwnRepositoryFork | LifecycleFailure::Conflict => {
                return (
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "You already own this gist or have a fork of it.".into(),
                )
            }
            LifecycleFailure::Denied => {
                return (
                    StatusCode::FORBIDDEN,
                    "This gist action is not permitted.".into(),
                )
            }
            LifecycleFailure::Invalid => {
                return (
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "Invalid gist files.".into(),
                )
            }
            _ => {}
        }
    }
    tracing::warn!(
        operation = "gist_native",
        code = "unavailable",
        "gist operation unavailable"
    );
    (
        StatusCode::SERVICE_UNAVAILABLE,
        "Gist storage is temporarily unavailable. Please try again.".into(),
    )
}
fn error(ctx: &PageContext, error: anyhow::Error) -> Response {
    let (status, message) = refusal(&error);
    ctx.error(status, &message)
}
fn number(ctx: &PageContext, key: &str) -> anyhow::Result<usize> {
    ctx.query.get(key).map_or(Ok(1), |page| {
        page.parse::<usize>()
            .ok()
            .filter(|page| (1..=10000).contains(page))
            .ok_or_else(|| InvalidInput("Page must be between 1 and 10000.").into())
    })
}
fn page_url(ctx: &PageContext, key: &str, page: usize) -> String {
    let mut query = ctx.query.clone();
    query.insert(key.into(), page.to_string());
    query.remove("saved");
    let mut pairs: Vec<_> = query.into_iter().collect();
    pairs.sort();
    format!(
        "{}?{}",
        ctx.path,
        url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(pairs)
            .finish()
    )
}
fn credential(ctx: &PageContext) -> GistCredential {
    GistCredential::Session(
        codeyard_api_http::session::extract_session_id(&ctx.headers).unwrap_or_default(),
    )
}
fn render_editor(ctx: &PageContext, status: StatusCode, editor: view::Editor) -> Response {
    ctx.page_model_status(
        status,
        if editor.id.is_some() {
            "Edit gist"
        } else {
            "Create gist"
        },
        AppPage::Gists(view::Page::Editor(editor)),
    )
}
