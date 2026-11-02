//! Native recipient workflow, backed by the same invitation transactions as the REST adapters.
use super::paging;
use crate::{
    backend::{
        api::http::context::PageContext,
        app::AppState,
        db,
        repositories::collaborators::{self, Error},
    },
    webui::{
        app::Page,
        pages::{account, repos::invitations as view},
    },
};
use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode, Uri},
    response::{IntoResponse, Redirect, Response},
    routing::{get, post},
    Form, Router,
};
use std::{collections::HashMap, sync::Arc};

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
    let limit = paging::DEFAULT_PER_PAGE;
    let mut rows = collaborators::invitations(
        &ctx.state.pool,
        actor,
        None,
        ((page - 1) * limit) as i64,
        (limit + 1) as i64,
    )
    .await?;
    let has_more = paging::has_next_page(page, rows.len(), limit, paging::MAX_PAGE);
    rows.truncate(limit);
    let mut invitations = Vec::new();
    for row in rows {
        let repository =
            db::repositories::records::get_repository_by_id(&ctx.state.pool, row.repo_id)
                .await
                .map_err(|_| Error::Unavailable)?
                .ok_or(Error::Unavailable)?;
        let owner = db::repositories::records::repository_owner_name(&ctx.state.pool, row.repo_id)
            .await
            .map_err(|_| Error::Unavailable)?
            .ok_or(Error::Unavailable)?;
        let inviter = db::users::accounts::get_user_by_id(&ctx.state.pool, row.inviter_id)
            .await
            .map_err(|_| Error::Unavailable)?
            .ok_or(Error::Unavailable)?;
        invitations.push(view::Invitation {
            id: row.id,
            owner,
            repository: repository.name,
            inviter: inviter.username,
            access: db::models::AccessLevel::from(row.permission)
                .as_str()
                .into(),
            created: row.created_at.to_rfc3339(),
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
    let page = match paging::number(&ctx, "page", 1, 1..=paging::MAX_PAGE) {
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
    match collaborators::respond(&ctx.state.pool, actor, id, accept).await {
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
