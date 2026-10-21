//! Administrator account management: the users page, signup requests, creating accounts, bans
//! and roles. Every handler rechecks administrator authority in its identity call.
use super::signup_requests::{load_requests, signup_decision};
use crate::backend::api::auth::errors;
use crate::backend::api::http::context::PageContext;
use crate::backend::api::http::error::{ApiError, ApiFailure};
use crate::backend::api::user::responses::{
    field, mutation_guard, optional_field, public_api_message, require_account, within_bounds,
    Fields,
};
use crate::backend::api::{auth, http::session};
use crate::backend::app::AppState;
use crate::backend::db;
use crate::backend::identity;
use crate::backend::identity::credentials::PasswordAccount;
use crate::webui::app::Page;
use crate::webui::pages::{
    account,
    admin::{AccountMetadata, AdminUsers},
    fields::preserved_values,
};
use axum::extract::{Form, Path, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::Response;
use axum::Json;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Accounts per page on the users page.
const USERS_PER_PAGE: usize = 25;

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
        false,
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

/// The users page's search, role filter and page position, read from a query string or a form.
struct Listing<'a> {
    search: &'a str,
    role: &'static str,
    page: usize,
}

/// The role filter: `admins`, `banned`, or empty for every account.
fn role_filter(value: &str) -> &'static str {
    match value {
        "admins" => "admins",
        "banned" => "banned",
        _ => "",
    }
}

fn listing_from_query(ctx: &PageContext) -> (String, &'static str, usize) {
    (
        ctx.query.get("q").cloned().unwrap_or_default(),
        role_filter(ctx.query.get("role").map_or("", String::as_str)),
        ctx.query
            .get("page")
            .and_then(|page| page.parse().ok())
            .unwrap_or(1),
    )
}

/// The users page address, with the search and page kept and the saved notice if any.
fn users_path(listing: &Listing, saved: &str) -> String {
    let mut query = url::form_urlencoded::Serializer::new(String::new());
    if !listing.search.is_empty() {
        query.append_pair("q", listing.search);
    }
    if !listing.role.is_empty() {
        query.append_pair("role", listing.role);
    }
    if listing.page > 1 {
        query.append_pair("page", &listing.page.to_string());
    }
    if !saved.is_empty() {
        query.append_pair("saved", saved);
    }
    let query = query.finish();
    if query.is_empty() {
        "/admin/users".to_owned()
    } else {
        format!("/admin/users?{query}")
    }
}

async fn load_users(ctx: &PageContext, listing: &Listing<'_>) -> Result<AdminUsers, Response> {
    let admin = require_account(ctx, true)?;
    let (all, admins, banned) =
        db::users::accounts::count_matching_users(&ctx.state.pool, listing.search)
            .await
            .map_err(|_| {
                ctx.error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Unable to load the accounts.",
                )
            })?;
    let total = match listing.role {
        "admins" => admins,
        "banned" => banned,
        _ => all,
    } as usize;
    let pages = total.div_ceil(USERS_PER_PAGE).max(1);
    let page = listing.page.clamp(1, pages);
    let accounts: Vec<AccountMetadata> = db::users::accounts::list_admin_users(
        &ctx.state.pool,
        listing.search,
        listing.role,
        USERS_PER_PAGE as i64,
        ((page - 1) * USERS_PER_PAGE) as i64,
    )
    .await
    .map_err(|_| {
        ctx.error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Unable to load the accounts.",
        )
    })?
    .into_iter()
    .map(|user| AccountMetadata {
        id: user.id,
        username: user.username,
        email: user.email.unwrap_or_default(),
        admin: user.is_admin,
        banned: user.is_banned,
        must_change_password: user.must_change_password,
        two_factor: user.two_factor_enabled,
        created: user.created_at.format("%b %e, %Y").to_string(),
    })
    .collect();
    let managed_accounts = db::node_config::get_node_configuration(&ctx.state.pool)
        .await
        .map_err(|_| {
            ctx.error(
                StatusCode::SERVICE_UNAVAILABLE,
                "Unable to load account settings.",
            )
        })?
        .is_some_and(|config| config.is_private());
    let (requests, requests_count, requests_page) = if managed_accounts {
        load_requests(ctx).await?
    } else {
        (Vec::new(), 0, 1)
    };
    Ok(AdminUsers {
        managed_accounts,
        current_user_id: admin.id,
        accounts,
        search: listing.search.trim().to_owned(),
        role: listing.role.to_owned(),
        counts: [all as usize, admins as usize, banned as usize],
        page,
        pages,
        total,
        requests,
        requests_count,
        requests_page,
    })
}

async fn users_response(
    ctx: &PageContext,
    listing: Listing<'_>,
    status: StatusCode,
    message: Option<&str>,
) -> Response {
    let mut ctx = ctx.clone();
    ctx.return_to = Some(users_path(&listing, ""));
    let ctx = &ctx;
    match load_users(ctx, &listing).await {
        Ok(model) => ctx.page_model_status(
            status,
            "Users",
            Page::Identity(account::Page::AdminUsers {
                model,
                csrf_token: ctx.csrf_token.clone(),
                message: message.map(str::to_owned),
                is_error: !status.is_success(),
            }),
        ),
        Err(response) => response,
    }
}

/// The form's search and page fields, so a refused action returns to the same list.
fn listing_from_form(form: &Fields) -> Listing<'_> {
    Listing {
        search: field(form, "q"),
        role: role_filter(field(form, "role")),
        page: field(form, "page").parse().unwrap_or(1),
    }
}

pub(crate) async fn users_get(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    let (search, role, page) = listing_from_query(&ctx);
    let message = match ctx.query.get("saved").map(String::as_str) {
        Some("ban") => Some("Account ban status updated."),
        Some("role") => Some("Administrator role updated."),
        Some("two-factor-reset") => Some("Two-factor authentication is off for the account."),
        Some("account") => Some("Managed account created. Share the temporary password securely."),
        Some("request") => Some("Signup request decision recorded."),
        _ => None,
    };
    users_response(
        &ctx,
        Listing {
            search: &search,
            role,
            page,
        },
        StatusCode::OK,
        message,
    )
    .await
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
    let result: Result<(), ApiFailure> = if !within_bounds(&form) {
        Err((
            StatusCode::BAD_REQUEST,
            Json(ApiError::new(
                "validation_error",
                "One or more fields exceed the supported length",
            )),
        ))
    } else {
        let session_id = session::extract_session_id(&ctx.headers);
        let email = optional_field(&form, "email");
        identity::accounts::create_managed_account(
            &ctx.state.pool,
            session_id.as_deref(),
            PasswordAccount {
                username: field(&form, "username"),
                email: email.as_deref(),
                password: field(&form, "password"),
            },
            field(&form, "is_admin") == "on",
        )
        .await
        .map(|_| ())
        .map_err(auth::errors::managed_account_failure)
    };
    match result {
        Ok(()) => ctx.redirect("/admin/users?saved=account"),
        Err(error) => {
            new_user_response(&ctx, &form, error.0, Some(public_api_message(&error.1 .0)))
        }
    }
}

fn new_user_response(
    ctx: &PageContext,
    submitted: &Fields,
    status: StatusCode,
    message: Option<&str>,
) -> Response {
    if let Err(response) = require_account(ctx, true) {
        return response;
    }
    ctx.page_model_status(
        status,
        "New user",
        Page::Identity(account::Page::AdminNewUser {
            values: preserved_values(submitted),
            csrf_token: ctx.csrf_token.clone(),
            message: message.map(str::to_owned),
            is_error: !status.is_success(),
        }),
    )
}

/// The new-user form. Only a private node creates accounts for people, so a public one sends
/// the administrator back to the list.
pub(crate) async fn new_user_get(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    if let Err(response) = require_account(&ctx, true) {
        return response;
    }
    match db::node_config::get_node_configuration(&ctx.state.pool).await {
        Ok(config) if config.as_ref().is_some_and(|config| config.is_private()) => {
            new_user_response(&ctx, &Fields::new(), StatusCode::OK, None)
        }
        Ok(_) => ctx.redirect("/admin/users"),
        Err(_) => ctx.error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Unable to load account settings.",
        ),
    }
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
    let listing = listing_from_form(&form);
    let session_id = session::extract_session_id(&ctx.headers);
    let result = identity::accounts::review_signup(
        &ctx.state.pool,
        session_id.as_deref(),
        id,
        signup_decision(field(&form, "action")),
    )
    .await
    .map_err(auth::errors::signup_review_failure);
    match result {
        Ok(_) => ctx.redirect(&users_path(&listing, "request")),
        Err(error) => {
            users_response(
                &ctx,
                listing,
                error.0,
                Some(public_api_message(&error.1 .0)),
            )
            .await
        }
    }
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
    let listing = listing_from_form(&form);
    let banned = match field(&form, "banned") {
        "true" if field(&form, "confirm_delete") == "yes" => true,
        "false" => false,
        _ => {
            return users_response(
                &ctx,
                listing,
                StatusCode::BAD_REQUEST,
                Some("Confirm the ban before saving it."),
            )
            .await
        }
    };
    let session_id = session::extract_session_id(&ctx.headers);
    let result = identity::accounts::set_ban(&ctx.state.pool, session_id.as_deref(), id, banned)
        .await
        .map_err(auth::errors::ban_failure);
    match result {
        Ok(_) => ctx.redirect(&users_path(&listing, "ban")),
        Err(error) => {
            users_response(
                &ctx,
                listing,
                error.0,
                Some(public_api_message(&error.1 .0)),
            )
            .await
        }
    }
}

pub(crate) async fn two_factor_reset_post(
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
    let listing = listing_from_form(&form);
    let session_id = session::extract_session_id(&ctx.headers);
    match identity::two_factor::admin_turn_off(&ctx.state.pool, session_id.as_deref(), id).await {
        Ok(()) => ctx.redirect(&users_path(&listing, "two-factor-reset")),
        Err(error) => {
            let (status, body) = auth::errors::two_factor_failure(error);
            users_response(&ctx, listing, status, Some(public_api_message(&body.0))).await
        }
    }
}

pub(crate) async fn role_post(
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
    let listing = listing_from_form(&form);
    let admin = match field(&form, "admin") {
        "true" => true,
        "false" => false,
        _ => {
            return users_response(
                &ctx,
                listing,
                StatusCode::BAD_REQUEST,
                Some("Choose whether the account is an administrator."),
            )
            .await
        }
    };
    let session_id = session::extract_session_id(&ctx.headers);
    match identity::accounts::set_admin_role(&ctx.state.pool, session_id.as_deref(), id, admin)
        .await
    {
        Ok(_) => ctx.redirect(&users_path(&listing, "role")),
        Err(error) => {
            let failure = errors::role_failure(error);
            users_response(
                &ctx,
                listing,
                failure.0,
                Some(public_api_message(&failure.1 .0)),
            )
            .await
        }
    }
}
