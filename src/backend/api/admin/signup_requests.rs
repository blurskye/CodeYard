use crate::backend::api::admin::accounts::{access_result, require_admin_user};
use crate::backend::api::auth::errors;
use crate::backend::api::auth::errors::database_failure;
use crate::backend::api::auth::setup::node_setup_status;
use crate::backend::api::http::context::PageContext;
use crate::backend::api::http::error::ApiError;
use crate::backend::api::user::responses::{
    field, mutation_guard, require_account, AuthResponse, AuthUserResponse as UserResponse, Fields,
};
use crate::backend::api::{auth, http::session};
use crate::backend::app::AppState;
use crate::backend::identity::accounts::SignupReviewOutcome;
use crate::backend::{db, identity};
use crate::webui::pages::admin::Signup;
use axum::extract::{Form, Path, Query, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::Response;
use axum::Json;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Debug, Serialize)]
pub struct SignupRequestResponse {
    pub id: i64,
    pub username: String,
    pub email: Option<String>,
    pub status: String,
    pub created_at: String,
}

#[derive(Debug, Deserialize)]
pub struct ResolveSignupRequest {
    pub request_id: i64,
    pub action: String,
}

#[derive(Default, Deserialize)]
pub struct SignupQuery {
    page: Option<usize>,
    after: Option<i64>,
}

pub async fn admin_list_signup_requests(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<SignupQuery>,
) -> Result<Json<Vec<SignupRequestResponse>>, (StatusCode, Json<ApiError>)> {
    let _admin = require_admin_user(&state, &headers).await?;
    let setup = node_setup_status(&state.pool).await?;
    if !setup.setup_completed || setup.node_mode != "private" {
        return Ok(Json(vec![]));
    }
    let requests = crate::backend::db::users::signup_requests::list_signup_requests(
        &state.pool,
        "pending",
        30,
        request_offset(query.page.unwrap_or(1)),
        query.after.unwrap_or(0),
    )
    .await
    .map_err(|_| database_failure())?;
    Ok(Json(
        requests
            .into_iter()
            .map(|request| SignupRequestResponse {
                id: request.id,
                username: request.username,
                email: request.email,
                status: request.status,
                created_at: request.created_at.to_rfc3339(),
            })
            .collect(),
    ))
}

pub async fn admin_resolve_signup_request(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<ResolveSignupRequest>,
) -> Result<Json<AuthResponse>, (StatusCode, Json<ApiError>)> {
    let session_id = session::extract_session_id(&headers);
    let outcome = identity::accounts::review_signup(
        &state.pool,
        session_id.as_deref(),
        req.request_id,
        signup_decision(&req.action),
    )
    .await
    .map_err(errors::signup_review_failure)?;
    let (user, message) = match outcome {
        SignupReviewOutcome::Approved(user) => {
            (Some(UserResponse::from(*user)), "Signup request approved")
        }
        SignupReviewOutcome::Rejected => (None, "Signup request rejected"),
    };
    Ok(Json(AuthResponse {
        success: true,
        user,
        message: Some(message.to_owned()),
    }))
}

pub(crate) fn signup_decision(action: &str) -> Option<identity::accounts::SignupDecision> {
    match action.trim().to_ascii_lowercase().as_str() {
        "approve" => Some(identity::accounts::SignupDecision::Approve),
        "reject" => Some(identity::accounts::SignupDecision::Reject),
        _ => None,
    }
}

fn request_offset(page: usize) -> i64 {
    i64::try_from(page.saturating_sub(1).saturating_mul(30)).unwrap_or(i64::MAX)
}

pub(crate) async fn load_requests(
    ctx: &PageContext,
    limit: i64,
) -> Result<(Vec<Signup>, usize, usize), Response> {
    // Callers must pass the admin + private-node guard before reading identities.
    require_account(ctx, true)?;
    let total = db::users::signup_requests::count_signup_requests(&ctx.state.pool, "pending")
        .await
        .map_err(|_| {
            ctx.error(
                StatusCode::SERVICE_UNAVAILABLE,
                "Unable to count signup requests.",
            )
        })? as usize;
    let page = ctx
        .query
        .get("request_page")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(1)
        .clamp(1, total.div_ceil(30).max(1));
    let after = match ctx.query.get("requests_after") {
        Some(value) => value
            .parse::<i64>()
            .ok()
            .filter(|value| *value >= 0)
            .ok_or_else(|| ctx.error(StatusCode::BAD_REQUEST, "Invalid signup request page."))?,
        None => 0,
    };
    let requests = db::users::signup_requests::list_signup_requests(
        &ctx.state.pool,
        "pending",
        limit,
        request_offset(page),
        after,
    )
    .await
    .map_err(|_| {
        ctx.error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Unable to load signup requests.",
        )
    })?;
    Ok((
        requests
            .into_iter()
            .map(|request| Signup {
                id: request.id,
                username: request.username,
                email: request.email.unwrap_or_default(),
                created: request.created_at.format("%b %e, %Y %H:%M UTC").to_string(),
            })
            .collect(),
        total,
        page,
    ))
}

pub(crate) async fn signup_post(
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
    let session_id = session::extract_session_id(&ctx.headers);
    let result = identity::accounts::review_signup(
        &ctx.state.pool,
        session_id.as_deref(),
        id,
        signup_decision(field(&form, "action")),
    )
    .await
    .map_err(auth::errors::signup_review_failure);
    access_result(&ctx, &uri, &form, result, "request").await
}
