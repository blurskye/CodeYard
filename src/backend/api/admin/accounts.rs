use crate::backend::api::admin::signup_requests::load_requests;
use crate::backend::api::auth::errors;
use crate::backend::api::auth::errors::database_failure;
use crate::backend::api::context::PageContext;
use crate::backend::api::error::ApiError;
use crate::backend::api::user::profile::{private_access, settings_result};
use crate::backend::api::user::responses::{
    field, mutation_guard, public_api_message, require_account, ApiFailure, AuthResponse,
    AuthUserResponse as UserResponse, Fields,
};
use crate::backend::api::{auth, session};
use crate::backend::app::AppState;
use crate::backend::{db, identity};
use crate::webui::pages::identity as view;
use axum::extract::{Form, Path, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::Response;
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
    headers: HeaderMap,
) -> Result<Json<Vec<AdminAccountResponse>>, (StatusCode, Json<ApiError>)> {
    let _admin = require_admin_user(&state, &headers).await?;
    let accounts = crate::backend::db::users::accounts::list_admin_account_summaries(&state.pool)
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

pub(crate) async fn load_admin(
    ctx: &PageContext,
    days: i32,
    submitted: Option<&Fields>,
) -> Result<view::Admin, Response> {
    let admin = require_account(ctx, true)?;
    let manage_access = private_access(ctx).await?;
    // Reads persisted facts only: repository sizes are recorded after pushes, never by a GET.
    let accounts = db::users::accounts::list_admin_account_summaries(&ctx.state.pool)
        .await
        .map_err(|_| {
            ctx.error(
                StatusCode::SERVICE_UNAVAILABLE,
                "Unable to load account statistics.",
            )
        })?
        .into_iter()
        .map(|account| view::Account {
            id: account.id,
            username: account.username,
            email: account.email.unwrap_or_default(),
            admin: account.is_admin,
            banned: account.is_banned,
            must_change_password: account.must_change_password,
            files: account.total_files,
            bytes: account.total_bytes,
            commits: account.commits_total,
            pushes: account.pushes_total,
            contributions: account.contributions_total,
            created: account.created_at.format("%b %e, %Y").to_string(),
        })
        .collect();
    let activity = db::users::accounts::list_admin_activity_points(&ctx.state.pool, days)
        .await
        .map_err(|_| {
            ctx.error(
                StatusCode::SERVICE_UNAVAILABLE,
                "Unable to load activity statistics.",
            )
        })?
        .into_iter()
        .map(|point| view::Activity {
            date: point.day.to_string(),
            contributions: point.daily_contributions,
            cumulative_contributions: point.cumulative_contributions,
            bytes: point.daily_storage_bytes,
            cumulative_bytes: point.cumulative_storage_bytes,
        })
        .collect();
    let requests = if manage_access {
        load_requests(ctx).await?
    } else {
        Vec::new()
    };
    let mut values = submitted.map(view::preserved_values).unwrap_or_default();
    values.days = days.to_string();
    let ai_crawlers = db::node_config::get_node_configuration(&ctx.state.pool)
        .await
        .map_err(|_| {
            ctx.error(
                StatusCode::SERVICE_UNAVAILABLE,
                "Unable to load the site settings.",
            )
        })?
        .map_or_else(
            || crate::backend::api::crawlers::BLOCK.to_owned(),
            |config| config.ai_crawlers,
        );
    Ok(view::Admin {
        current_user_id: admin.id,
        accounts,
        activity,
        days,
        manage_access,
        requests,
        values,
        ai_crawlers,
    })
}

pub(crate) async fn admin_response(
    ctx: &PageContext,
    days: i32,
    submitted: Option<&Fields>,
    status: StatusCode,
    message: Option<&str>,
) -> Response {
    match load_admin(ctx, days, submitted).await {
        Ok(model) => ctx.page_model_status(
            status,
            "Admin",
            crate::webui::app::Page::Identity(view::Page::Admin {
                model,
                csrf_token: ctx.csrf_token.clone(),
                message: message.map(str::to_owned),
                is_error: !status.is_success(),
            }),
        ),
        Err(response) => response,
    }
}

pub(crate) async fn admin_get(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    let days = view::normalize_days(ctx.query.get("days").map(String::as_str));
    let message = match ctx.query.get("saved").map(String::as_str) {
        Some("ban") => Some("Account ban status updated."),
        Some("account") => Some("Managed account created. Share the temporary password securely."),
        Some("request") => Some("Signup request decision recorded."),
        Some("crawlers") => Some("AI crawler setting saved."),
        _ => None,
    };
    admin_response(&ctx, days, None, StatusCode::OK, message).await
}

pub(crate) async fn access_result<T>(
    ctx: &PageContext,
    uri: &Uri,
    form: &Fields,
    result: Result<T, ApiFailure>,
    saved: &str,
) -> Response {
    if uri.path().starts_with("/admin/") {
        let days = view::normalize_days(form.get("days").map(String::as_str));
        match result {
            Ok(_) => ctx.redirect(&format!("/admin?days={days}&saved={saved}")),
            Err(error) => {
                admin_response(
                    ctx,
                    days,
                    Some(form),
                    error.0,
                    Some(public_api_message(&error.1 .0)),
                )
                .await
            }
        }
    } else {
        settings_result(ctx, "access", form, result, saved).await
    }
}

pub(crate) async fn ban_get(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
    Path(id): Path<i64>,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    let admin = match require_account(&ctx, true) {
        Ok(user) => user,
        Err(response) => return response,
    };
    let target = match db::users::accounts::get_user_by_id(&ctx.state.pool, id).await {
        Ok(Some(user)) if user.id != admin.id && !user.is_banned => user,
        Ok(_) => return ctx.error(StatusCode::NOT_FOUND, "Account not found."),
        Err(_) => {
            return ctx.error(
                StatusCode::SERVICE_UNAVAILABLE,
                "Unable to load this account.",
            )
        }
    };
    ctx.page_model(
        "Confirm account ban",
        crate::webui::app::Page::Identity(view::Page::BanConfirmation {
            username: target.username,
            user_id: id,
            csrf_token: ctx.csrf_token.clone(),
            days: view::normalize_days(ctx.query.get("days").map(String::as_str)),
        }),
    )
}

pub(crate) async fn ban_post(
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
    if let Err(response) = mutation_guard(&ctx, &form, true) {
        return response;
    }
    let days = view::normalize_days(form.get("days").map(String::as_str));
    let banned = match field(&form, "banned") {
        "true" if field(&form, "confirm_delete") == "yes" => true,
        "false" => false,
        _ => {
            return admin_response(
                &ctx,
                days,
                Some(&form),
                StatusCode::BAD_REQUEST,
                Some("Confirm deletion of the account's owned repositories before banning."),
            )
            .await
        }
    };
    let session_id = session::extract_session_id(&ctx.headers);
    let result = identity::accounts::set_ban(&ctx.state.pool, session_id.as_deref(), id, banned)
        .await
        .map_err(auth::errors::ban_failure);
    match result {
        Ok(_) => ctx.redirect(&format!("/admin?days={days}&saved=ban")),
        Err(error) => {
            admin_response(
                &ctx,
                days,
                Some(&form),
                error.0,
                Some(public_api_message(&error.1 .0)),
            )
            .await
        }
    }
}
