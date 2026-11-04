use crate::user::profile::settings_response;
use crate::user::responses::mutation_guard;
use crate::user::responses::require_account;
use axum::extract::Form;
use axum::extract::Path;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::http::Uri;
use axum::response::Response;
use codeyard_api_http::context::PageContext;
use codeyard_api_http::shared::user::responses::field;
use codeyard_api_http::shared::user::responses::Fields;
use codeyard_app::app::AppState;
use codeyard_db_core::queries::users::accounts::AccountCredential;
use codeyard_identity::gpg::GpgError;
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
    let session = codeyard_api_http::session::extract_session_id(&ctx.headers).unwrap_or_default();
    result(
        &ctx,
        codeyard_identity::gpg::enroll(
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
    let session = codeyard_api_http::session::extract_session_id(&ctx.headers).unwrap_or_default();
    result(
        &ctx,
        codeyard_identity::gpg::delete(
            &ctx.state.pool,
            actor.id,
            &AccountCredential::Session(&session),
            id,
        )
        .await,
    )
    .await
}
