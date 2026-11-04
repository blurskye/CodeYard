//! Native gist pages and forms share the Git-backed domain with the GitHub API.
use crate::backend::{
    api::{
        http::{context::PageContext, session},
        user::responses::{field, mutation_guard, Fields},
    },
    app::AppState,
    db, gists as domain,
    git::error::{ErrorKind, RepoError},
    repositories::{
        lifecycle::{LifecycleFailure, StorageError},
        store::{repo_store, GistCredential},
    },
};
use axum::{
    extract::{DefaultBodyLimit, Form, Path, State},
    http::{header, HeaderMap, StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::{get, post},
    Router,
};
use codeyard_webui::{app::Page as AppPage, pages::gists as view};
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
        .route("/gists", get(pages::list))
        .route("/gists/new", get(pages::new).post(forms::create))
        .route("/gists/:id", get(pages::read))
        .route("/gists/:id/edit", get(pages::edit).post(forms::edit))
        .route("/gists/:id/raw/:sha/:filename", get(pages::raw))
        .route("/gists/:id/star", post(forms::star))
        .route("/gists/:id/fork", post(forms::fork))
        .route("/gists/:id/delete", post(forms::delete))
        .route("/gists/:id/comments", post(forms::comment))
        .route(
            "/gists/:id/comments/:comment/edit",
            post(forms::edit_comment),
        )
        .route(
            "/gists/:id/comments/:comment/delete",
            post(forms::delete_comment),
        )
        .layer(DefaultBodyLimit::max(4 * 1024 * 1024))
}
fn refusal(error: &anyhow::Error) -> (StatusCode, String) {
    if let Some(error) = error.downcast_ref::<InvalidInput>() {
        return (StatusCode::UNPROCESSABLE_ENTITY, error.to_string());
    }
    if let Some(error) = error.downcast_ref::<db::gists::GistRefusal>() {
        return (
            match error {
                db::gists::GistRefusal::Missing => StatusCode::NOT_FOUND,
                db::gists::GistRefusal::Denied => StatusCode::FORBIDDEN,
                db::gists::GistRefusal::Conflict => StatusCode::CONFLICT,
                db::gists::GistRefusal::Invalid => StatusCode::UNPROCESSABLE_ENTITY,
            },
            error.to_string(),
        );
    }
    if error.is::<codeyard_support::credentials::AccountRestricted>() {
        return (
            StatusCode::FORBIDDEN,
            "Your session no longer permits this change. Sign in again.".into(),
        );
    }
    if error.is::<domain::GistSaved>() || error.is::<domain::GistDeleted>() {
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
    GistCredential::Session(session::extract_session_id(&ctx.headers).unwrap_or_default())
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
