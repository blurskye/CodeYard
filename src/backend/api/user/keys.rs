use crate::backend::api::http::context::PageContext;
use crate::backend::api::http::error::ApiError;
use crate::backend::api::http::session::extract_session_id;
use crate::backend::api::user::profile::settings_response;
use crate::backend::api::user::profile::settings_result;
use crate::backend::api::user::responses::field;
use crate::backend::api::user::responses::mutation_guard;
use crate::backend::api::user::responses::within_bounds;
use crate::backend::api::user::responses::Fields;
use crate::backend::app::AppState;

use crate::backend::identity::keys::KeyError;
use axum::extract::{Form, Path, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::Response;
use axum::Json;
use codeyard_webui::pages::fields::may_be_private_key;
use std::sync::Arc;

pub(crate) fn key_failure(error: KeyError) -> (StatusCode, Json<ApiError>) {
    let (status, code) = match error {
        KeyError::InvalidName | KeyError::InvalidKey => {
            (StatusCode::BAD_REQUEST, "validation_error")
        }
        KeyError::AlreadyUsed => (StatusCode::CONFLICT, "key_already_registered"),
        KeyError::AccountRestricted | KeyError::InvalidSession => {
            (StatusCode::FORBIDDEN, "forbidden")
        }
        KeyError::Missing => (StatusCode::NOT_FOUND, "not_found"),
        KeyError::Unavailable => (StatusCode::SERVICE_UNAVAILABLE, "unavailable"),
    };
    (status, Json(ApiError::new(code, &error.to_string())))
}

#[expect(
    clippy::result_large_err,
    reason = "HTML adapters preserve complete Axum responses under the PageContext contract."
)]
fn key_mutation_actor(ctx: &PageContext, form: &Fields) -> Result<i64, Response> {
    mutation_guard(ctx, form, false)?;
    let user = ctx.user.as_ref().expect("mutation guard checked account");
    if user.must_change_password {
        return Err(ctx.error(
            StatusCode::FORBIDDEN,
            "Change your temporary password before managing SSH keys.",
        ));
    }
    Ok(user.id)
}

pub(crate) async fn key_add_post(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
    Form(form): Form<Fields>,
) -> Response {
    save_key_form(state, headers, uri, form, false).await
}

pub(crate) async fn signing_key_add_post(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
    Form(form): Form<Fields>,
) -> Response {
    save_key_form(state, headers, uri, form, true).await
}

async fn save_key_form(
    state: Arc<AppState>,
    headers: HeaderMap,
    uri: Uri,
    form: Fields,
    signing: bool,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    let actor = match key_mutation_actor(&ctx, &form) {
        Ok(actor) => actor,
        Err(response) => return response,
    };
    if !within_bounds(&form) {
        return settings_response(
            &ctx,
            "keys",
            Some(&form),
            StatusCode::BAD_REQUEST,
            Some("The SSH key or its title exceeds the supported length."),
        )
        .await;
    }
    let public_key = field(&form, "public_key").trim();
    if may_be_private_key(public_key) {
        // The refused page keeps the rest of the draft; preserved_values drops the private key.
        return settings_response(
            &ctx,
            "keys",
            Some(&form),
            StatusCode::BAD_REQUEST,
            Some("Paste only your public key, never a private key."),
        )
        .await;
    }
    let session_id = extract_session_id(&ctx.headers);
    let result = if signing {
        crate::backend::identity::keys::enroll_signing(
            &ctx.state.pool,
            actor,
            &crate::backend::db::users::accounts::AccountCredential::Session(
                session_id.as_deref().unwrap_or_default(),
            ),
            field(&form, "name").trim(),
            public_key,
        )
        .await
        .map(|_| ())
    } else {
        crate::backend::identity::keys::enroll(
            &ctx.state.pool,
            actor,
            session_id.as_deref(),
            field(&form, "name").trim(),
            public_key,
        )
        .await
        .map(|_| ())
    }
    .map_err(key_failure);
    settings_result(
        &ctx,
        "keys",
        &form,
        result,
        if signing { "signing-key" } else { "key" },
    )
    .await
}

pub(crate) async fn signing_key_delete_post(
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
    let actor = match key_mutation_actor(&ctx, &form) {
        Ok(actor) => actor,
        Err(response) => return response,
    };
    let session = extract_session_id(&ctx.headers).unwrap_or_default();
    let result = crate::backend::identity::keys::delete_signing(
        &ctx.state.pool,
        actor,
        &crate::backend::db::users::accounts::AccountCredential::Session(&session),
        id,
    )
    .await
    .map_err(key_failure);
    settings_result(&ctx, "keys", &form, result, "signing-key-deleted").await
}

pub(crate) async fn key_delete_post(
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
    let actor = match key_mutation_actor(&ctx, &form) {
        Ok(actor) => actor,
        Err(response) => return response,
    };
    let session = extract_session_id(&ctx.headers).unwrap_or_default();
    let result = crate::backend::identity::keys::delete(
        &ctx.state.pool,
        actor,
        &crate::backend::db::users::accounts::AccountCredential::Session(&session),
        id,
    )
    .await
    .map_err(key_failure);
    settings_result(&ctx, "keys", &form, result, "deleted").await
}
