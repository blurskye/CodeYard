use super::{
    profile::settings_response,
    responses::{field, mutation_guard, require_account, Fields},
};
use crate::backend::{
    api::http::{context::PageContext, session},
    app::AppState,
    db::users::accounts::AccountCredential,
    identity::gpg::{self, GpgError},
};
use axum::{
    extract::{Form, Path, State},
    http::{HeaderMap, StatusCode, Uri},
    response::Response,
};
use std::sync::Arc;

async fn result(ctx: &PageContext, result: Result<(), GpgError>) -> Response {
    match result {
        Ok(()) => ctx.redirect("/settings?tab=keys&saved=gpg-key"),
        Err(error) => {
            let status = match error {
                GpgError::Denied => StatusCode::FORBIDDEN,
                GpgError::Missing => StatusCode::NOT_FOUND,
                GpgError::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
                _ => StatusCode::UNPROCESSABLE_ENTITY,
            };
            // Never echo an accidentally pasted private key or malformed armor.
            settings_response(ctx, "keys", None, status, Some(&error.to_string())).await
        }
    }
}
pub(crate) async fn add_post(
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
    result(
        &ctx,
        gpg::enroll(
            &ctx.state.pool,
            actor.id,
            &AccountCredential::Session(&session),
            Some(field(&form, "gpg_name").trim()),
            field(&form, "armored_public_key"),
        )
        .await
        .map(|_| ()),
    )
    .await
}
pub(crate) async fn delete_post(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
    Path(id): Path<i64>,
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
    result(
        &ctx,
        gpg::delete(
            &ctx.state.pool,
            actor.id,
            &AccountCredential::Session(&session),
            id,
        )
        .await,
    )
    .await
}
