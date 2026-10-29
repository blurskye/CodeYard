use super::{
    profile::settings_response,
    responses::{field, mutation_guard, require_account, Fields},
};
use crate::backend::{
    api::http::{context::PageContext, session},
    app::AppState,
    db::users::accounts::AccountCredential,
    identity::blocks::{self, BlockError},
};
use axum::{
    extract::{Form, State},
    http::{HeaderMap, StatusCode, Uri},
    response::Response,
};
use std::sync::Arc;

pub(crate) async fn blocks_post(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
    Form(form): Form<Fields>,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    if let Err(response) = mutation_guard(&ctx, &form, false) {
        return response;
    }
    let actor = match require_account(&ctx, false) {
        Ok(actor) => actor,
        Err(response) => return response,
    };
    let session = session::extract_session_id(&ctx.headers).unwrap_or_default();
    match blocks::change(
        &ctx.state.pool,
        actor.id,
        &AccountCredential::Session(&session),
        field(&form, "username").trim(),
        field(&form, "unblock") != "yes",
    )
    .await
    {
        Ok(()) => ctx.redirect("/settings?tab=blocked&saved=blocks"),
        Err(error) => {
            let status = match error {
                BlockError::Missing => StatusCode::NOT_FOUND,
                BlockError::SelfBlock => StatusCode::UNPROCESSABLE_ENTITY,
                BlockError::Denied => StatusCode::FORBIDDEN,
                BlockError::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
            };
            settings_response(&ctx, "blocked", None, status, Some(&error.to_string())).await
        }
    }
}
