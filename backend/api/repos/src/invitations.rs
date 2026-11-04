//! Native recipient workflow, backed by the same invitation transactions as the REST adapters.
use axum::response::IntoResponse;

use axum::extract::Path;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::http::Uri;
use axum::response::Redirect;
use axum::response::Response;
use axum::routing::get;
use axum::routing::post;
use axum::Form;
use axum::Router;
use codeyard_api_http::context::PageContext;
use codeyard_app::app::AppState;
use codeyard_repos::collaborators::Error;
use codeyard_webui::app::Page;
use codeyard_webui::pages::account;
use codeyard_webui::pages::repos::invitations as view;
use std::collections::HashMap;
use std::sync::Arc;

fn refusal(ctx: &PageContext, error: Error) -> Response {
    match error {
        Error::Denied => ctx.error(
            StatusCode::FORBIDDEN,
            "Your permissions changed. Reload your invitations before retrying.",
        ),
        Error::NotFound => ctx.error(StatusCode::NOT_FOUND, "Invitation not found or expired."),
        Error::Invalid(message) => ctx.error(StatusCode::UNPROCESSABLE_ENTITY, &message),
        Error::Unavailable => ctx.error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Invitation storage is unavailable. Reload your invitations before retrying.",
        ),
    }
}
async fn load(ctx: &PageContext, actor: i64, page: usize) -> Result<view::Page, Error> {
    let limit = crate::paging::DEFAULT_PER_PAGE;
    let session = codeyard_api_http::session::extract_session_id(&ctx.headers).unwrap_or_default();
    let mut rows = codeyard_repos::collaborators::recipient_invitations(
        &ctx.state.pool,
        actor,
        &session,
        ((page - 1) * limit) as i64,
        (limit + 1) as i64,
    )
    .await?;
    let has_more = crate::paging::has_next_page(page, rows.len(), limit, crate::paging::MAX_PAGE);
    rows.truncate(limit);
    let mut invitations = Vec::new();
    for row in rows {
        invitations.push(view::Invitation {
            id: row.invitation.id,
            owner: row.owner,
            repository: row.repository,
            inviter: row.inviter,
            access: codeyard_db_core::models::AccessLevel::from(row.invitation.permission)
                .as_str()
                .into(),
            created: row.invitation.created_at.to_rfc3339(),
        });
    }
    Ok(view::Page {
        csrf_token: ctx.csrf_token.clone(),
        invitations,
        page,
        has_more,
    })
}
async fn list(State(state): State<Arc<AppState>>, headers: HeaderMap, uri: Uri) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    let actor = match ctx.require_user() {
        Ok(user) => user.id,
        Err(response) => return response,
    };
    let page = match crate::paging::number(&ctx, "page", 1, 1..=crate::paging::MAX_PAGE) {
        Ok(page) => page,
        Err(response) => return response,
    };
    match load(&ctx, actor, page).await {
        Ok(page) => ctx.page_model(
            "Repository invitations",
            Page::Identity(account::Page::RepositoryInvitations { page }),
        ),
        Err(error) => refusal(&ctx, error),
    }
}
async fn respond(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
    Path((id, action)): Path<(i64, String)>,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    if let Err(response) = ctx.check_csrf(&form) {
        return response;
    }
    let actor = match ctx.require_user() {
        Ok(user) => user.id,
        Err(response) => return response,
    };
    let accept = match action.as_str() {
        "accept" => true,
        "decline" => false,
        _ => return ctx.error(StatusCode::NOT_FOUND, "Unknown invitation action."),
    };
    let authority = ctx.database_actor(actor);
    match codeyard_repos::collaborators::respond(&ctx.state.pool, authority, id, accept)
        .await
    {
        Ok(()) => ctx.redirect("/settings/invitations"),
        Err(error) => refusal(&ctx, error),
    }
}
async fn alias() -> Response {
    Redirect::to("/settings/invitations").into_response()
}
pub(super) fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/settings/invitations", get(list))
        .route("/settings/invitations/:id/:action", post(respond))
        .route("/:owner/:repo/invitations", get(alias))
        .layer(axum::extract::DefaultBodyLimit::max(16 * 1024))
}
