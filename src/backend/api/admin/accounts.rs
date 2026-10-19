use crate::backend::api::admin::signup_requests::load_requests;
use crate::backend::api::auth::errors;
use crate::backend::api::auth::errors::database_failure;
use crate::backend::api::http::context::PageContext;
use crate::backend::api::http::error::ApiError;
use crate::backend::api::http::session;
use crate::backend::api::user::profile::{private_access, settings_result};
use crate::backend::api::user::responses::{
    public_api_message, require_account, ApiFailure, AuthResponse,
    AuthUserResponse as UserResponse, Fields,
};
use crate::backend::app::AppState;
use crate::backend::{db, identity};
use crate::webui::pages::{
    account,
    admin::{normalize_days, Account, Activity, Admin},
    fields::preserved_values,
};
use axum::extract::{Path, State};
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

fn overview_accounts(
    mut accounts: Vec<db::models::AdminAccountSummary>,
) -> (Vec<Account>, (usize, usize, i64)) {
    let totals = (
        accounts.len(),
        accounts.iter().filter(|account| account.is_banned).count(),
        accounts.iter().map(|account| account.total_bytes).sum(),
    );
    accounts.sort_by(|a, b| {
        b.created_at
            .cmp(&a.created_at)
            .then_with(|| b.id.cmp(&a.id))
    });
    accounts.truncate(5);
    (accounts.into_iter().map(account_row).collect(), totals)
}

/// The account as the admin pages show it.
pub(crate) fn account_row(account: db::models::AdminAccountSummary) -> Account {
    Account {
        id: account.id,
        username: account.username,
        email: account.email.unwrap_or_default(),
        admin: account.is_admin,
        banned: account.is_banned,
        must_change_password: account.must_change_password,
        two_factor: account.two_factor_enabled,
        files: account.total_files,
        bytes: account.total_bytes,
        commits: account.commits_total,
        pushes: account.pushes_total,
        contributions: account.contributions_total,
        created: account.created_at.format("%b %e, %Y").to_string(),
    }
}

pub(crate) async fn load_admin(
    ctx: &PageContext,
    days: i32,
    submitted: Option<&Fields>,
) -> Result<Admin, Response> {
    let admin = require_account(ctx, true)?;
    let manage_access = private_access(ctx).await?;
    // Reads persisted facts only: repository sizes are recorded after pushes, never by a GET.
    let (accounts, totals) = overview_accounts(
        db::users::accounts::list_admin_account_summaries(&ctx.state.pool)
            .await
            .map_err(|_| {
                ctx.error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Unable to load account statistics.",
                )
            })?,
    );
    let activity = db::users::accounts::list_admin_activity_points(&ctx.state.pool, days)
        .await
        .map_err(|_| {
            ctx.error(
                StatusCode::SERVICE_UNAVAILABLE,
                "Unable to load activity statistics.",
            )
        })?
        .into_iter()
        .map(|point| Activity {
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
    let mut values = submitted.map(preserved_values).unwrap_or_default();
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
    Ok(Admin {
        current_user_id: admin.id,
        accounts,
        totals,
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
            crate::webui::app::Page::Identity(account::Page::Admin {
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
    let days = normalize_days(ctx.query.get("days").map(String::as_str));
    let message = match ctx.query.get("saved").map(String::as_str) {
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
        let days = normalize_days(form.get("days").map(String::as_str));
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn overview_transfers_only_five_recent_accounts_with_full_totals() {
        let rows = (0..1000)
            .map(|id| db::models::AdminAccountSummary {
                id,
                username: format!("user-{id}"),
                email: None,
                is_admin: false,
                is_banned: id % 2 == 0,
                must_change_password: false,
                two_factor_enabled: false,
                created_at: chrono::DateTime::from_timestamp(id, 0).unwrap(),
                total_files: 0,
                total_bytes: 10,
                commits_total: 0,
                pushes_total: 0,
                contributions_total: 0,
            })
            .collect();
        let (recent, totals) = overview_accounts(rows);
        assert_eq!(totals, (1000, 500, 10_000));
        assert_eq!(recent.len(), 5);
        assert_eq!(recent[0].id, 999);
        assert_eq!(recent[4].id, 995);
    }
}
