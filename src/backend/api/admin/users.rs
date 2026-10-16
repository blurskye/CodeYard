use crate::backend::api::admin::accounts::access_result;
use crate::backend::api::auth::errors;
use crate::backend::api::context::PageContext;
use crate::backend::api::error::ApiError;
use crate::backend::api::user::responses::{
    field, mutation_guard, optional_field, within_bounds, Fields,
};
use crate::backend::api::{auth, session};
use crate::backend::app::AppState;
use crate::backend::identity;
use crate::backend::identity::credentials::PasswordAccount;
use axum::extract::{Form, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::Response;
use axum::Json;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Debug, Deserialize)]
pub struct CreateManagedUserRequest {
    pub username: String,
    pub email: Option<String>,
    pub password: String,
}

#[derive(Debug, Serialize)]
pub struct ManagedUserResponse {
    pub id: i64,
    pub username: String,
    pub email: Option<String>,
    pub must_change_password: bool,
}

pub async fn admin_create_user(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<CreateManagedUserRequest>,
) -> Result<(StatusCode, Json<ManagedUserResponse>), (StatusCode, Json<ApiError>)> {
    let session_id = session::extract_session_id(&headers);
    let user = identity::accounts::create_managed_account(
        &state.pool,
        session_id.as_deref(),
        PasswordAccount {
            username: &req.username,
            email: req.email.as_deref(),
            password: &req.password,
        },
    )
    .await
    .map_err(errors::managed_account_failure)?;
    Ok((
        StatusCode::CREATED,
        Json(ManagedUserResponse {
            id: user.id,
            username: user.username,
            email: user.email,
            must_change_password: user.must_change_password,
        }),
    ))
}

pub(crate) async fn managed_user_post(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
    Form(form): Form<Fields>,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    if let Err(response) = mutation_guard(&ctx, &form, true) {
        return response;
    }
    if !within_bounds(&form) {
        let failure = (
            StatusCode::BAD_REQUEST,
            Json(ApiError::new(
                "validation_error",
                "One or more fields exceed the supported length",
            )),
        );
        return access_result::<()>(&ctx, &uri, &form, Err(failure), "account").await;
    }
    let session_id = session::extract_session_id(&ctx.headers);
    let email = optional_field(&form, "email");
    let result = identity::accounts::create_managed_account(
        &ctx.state.pool,
        session_id.as_deref(),
        PasswordAccount {
            username: field(&form, "username"),
            email: email.as_deref(),
            password: field(&form, "password"),
        },
    )
    .await
    .map_err(auth::errors::managed_account_failure);
    access_result(&ctx, &uri, &form, result, "account").await
}
