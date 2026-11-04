use crate::backend::api::http::context::PageContext;
use crate::backend::api::user::profile::settings_response;
use crate::backend::api::user::responses::field;
use crate::backend::api::user::responses::mutation_guard;
use crate::backend::api::user::responses::require_account;
use crate::backend::api::user::responses::Fields;

use crate::backend::app::AppState;
use codeyard_db_core::queries::users::accounts::AccountCredential;

use axum::{
    extract::{Form, State},
    http::{HeaderMap, StatusCode, Uri},
    response::Response,
};
use codeyard_identity::blocks::BlockError;
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
    let session =
        crate::backend::api::http::session::extract_session_id(&ctx.headers).unwrap_or_default();
    match codeyard_identity::blocks::change(
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
