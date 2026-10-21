use crate::backend::api::auth::errors;
use crate::backend::api::auth::errors::database_failure;
use crate::backend::api::http::error::ApiError;
use crate::backend::api::http::session;
use crate::backend::api::user::responses::{AuthResponse, AuthUserResponse as UserResponse};
use crate::backend::app::AppState;
use crate::backend::{db, identity};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Debug, Serialize)]
pub struct AdminAccountResponse {
    pub id: i64,
    pub username: String,
    pub email: Option<String>,
    pub is_admin: bool,
    pub is_banned: bool,
    pub must_change_password: bool,
    pub created_at: String,
    pub total_files: i64,
    pub total_bytes: i64,
    pub commits_total: i64,
    pub pushes_total: i64,
    pub contributions_total: i64,
}

#[derive(Debug, Deserialize)]
pub struct SetUserBanRequest {
    pub banned: bool,
}

pub async fn admin_list_accounts(
    State(state): State<Arc<AppState>>,
    Query(query): Query<std::collections::HashMap<String, String>>,
    headers: HeaderMap,
) -> Result<Json<Vec<AdminAccountResponse>>, (StatusCode, Json<ApiError>)> {
    let _admin = require_admin_user(&state, &headers).await?;
    let page = query
        .get("page")
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(1)
        .clamp(1, 10_000);
    let accounts = crate::backend::db::users::accounts::list_admin_account_summaries(
        &state.pool,
        100,
        i64::from(page - 1) * 100,
    )
    .await
    .map_err(|_| database_failure())?;
    Ok(Json(
        accounts
            .into_iter()
            .map(|account| AdminAccountResponse {
                id: account.id,
                username: account.username,
                email: account.email,
                is_admin: account.is_admin,
                is_banned: account.is_banned,
                must_change_password: account.must_change_password,
                created_at: account.created_at.to_rfc3339(),
                total_files: account.total_files,
                total_bytes: account.total_bytes,
                commits_total: account.commits_total,
                pushes_total: account.pushes_total,
                contributions_total: account.contributions_total,
            })
            .collect(),
    ))
}

pub async fn admin_set_user_ban(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(user_id): Path<i64>,
    Json(req): Json<SetUserBanRequest>,
) -> Result<Json<AuthResponse>, (StatusCode, Json<ApiError>)> {
    let session_id = session::extract_session_id(&headers);
    let user = identity::accounts::set_ban(&state.pool, session_id.as_deref(), user_id, req.banned)
        .await
        .map_err(errors::ban_failure)?;
    Ok(Json(AuthResponse {
        success: true,
        user: Some(UserResponse::from(user)),
        message: Some(if req.banned {
            "User banned; repositories and stored data preserved".to_owned()
        } else {
            "User unbanned".to_owned()
        }),
    }))
}

pub(crate) async fn require_admin_user(
    state: &Arc<AppState>,
    headers: &HeaderMap,
) -> Result<db::models::User, (StatusCode, Json<ApiError>)> {
    let user = session::get_current_user(&state.pool, headers)
        .await
        .map_err(|(status, body)| (status, Json(ApiError::new(&body.0.error, &body.0.message))))?;
    if !user.is_admin || user.must_change_password {
        return Err((
            StatusCode::FORBIDDEN,
            Json(ApiError::new("forbidden", "Admin access required")),
        ));
    }

    Ok(user)
}
