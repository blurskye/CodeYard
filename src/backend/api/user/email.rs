//! Changing the account's email address. The current password is required.

use crate::backend::api::auth::errors::email_failure;
use crate::backend::api::http::context::PageContext;
use crate::backend::api::http::session;
use crate::backend::api::user::profile::settings_result;
use crate::backend::api::user::responses::{field, mutation_guard, Fields};
use crate::backend::app::AppState;
use crate::backend::identity;
use axum::extract::{Form, State};
use axum::http::{HeaderMap, Uri};
use axum::response::Response;
use std::sync::Arc;

pub(crate) async fn email_post(
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
    let session_id = session::extract_session_id(&ctx.headers);
    let result = identity::accounts::change_email(
        &ctx.state.pool,
        session_id.as_deref(),
        field(&form, "current_password"),
        field(&form, "email"),
    )
    .await
    .map_err(email_failure);
    settings_result(&ctx, "email", &form, result, "email").await
}
