//! Administrator account management: the users page, creating accounts, bans and roles.
//! Every handler rechecks administrator authority in its identity call.
use crate::backend::api::auth::errors;
use crate::backend::api::http::context::PageContext;
use crate::backend::api::http::error::ApiError;
use crate::backend::api::user::profile::settings_result;
use crate::backend::api::user::responses::{
    field, mutation_guard, optional_field, public_api_message, require_account, within_bounds,
    ApiFailure, Fields,
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

/// The users page's search and page position, read from a query string or a form.
struct Listing<'a> {
    search: &'a str,
    page: usize,
}

fn listing_from_query(ctx: &PageContext) -> (String, usize) {
    (
        ctx.query.get("q").cloned().unwrap_or_default(),
        ctx.query
            .get("page")
            .and_then(|page| page.parse().ok())
            .unwrap_or(1),
    )
}

/// The users page address after an action, with the search and page kept.
fn users_path(listing: &Listing, saved: &str) -> String {
    let mut query = url::form_urlencoded::Serializer::new(String::new());
    if !listing.search.is_empty() {
        query.append_pair("q", listing.search);
    }
    if listing.page > 1 {
        query.append_pair("page", &listing.page.to_string());
    }
    query.append_pair("saved", saved);
    format!("/admin/users?{}", query.finish())
}

async fn load_users(
    ctx: &PageContext,
    search: &str,
    page: usize,
    submitted: Option<&Fields>,
) -> Result<AdminUsers, Response> {
    let admin = require_account(ctx, true)?;
    let total = db::users::accounts::count_matching_users(&ctx.state.pool, search)
        .await
        .map_err(|_| {
            ctx.error(
                StatusCode::SERVICE_UNAVAILABLE,
                "Unable to load the accounts.",
            )
        })? as usize;
    let pages = total.div_ceil(USERS_PER_PAGE).max(1);
    let page = page.clamp(1, pages);
    let accounts: Vec<AccountMetadata> = db::users::accounts::list_admin_users(
        &ctx.state.pool,
        search,
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
    Ok(AdminUsers {
        current_user_id: admin.id,
        accounts,
        search: search.trim().to_owned(),
        page,
        pages,
        total,
        values: submitted.map(preserved_values).unwrap_or_default(),
    })
}

async fn users_response(
    ctx: &PageContext,
    listing: Listing<'_>,
    submitted: Option<&Fields>,
    status: StatusCode,
    message: Option<&str>,
) -> Response {
    match load_users(ctx, listing.search, listing.page, submitted).await {
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
    let (search, page) = listing_from_query(&ctx);
    let message = match ctx.query.get("saved").map(String::as_str) {
        Some("ban") => Some("Account ban status updated."),
        Some("role") => Some("Administrator role updated."),
        Some("two-factor-reset") => Some("Two-factor authentication is off for the account."),
        Some("account") => Some("Managed account created. Share the temporary password securely."),
        _ => None,
    };
    users_response(
        &ctx,
        Listing {
            search: &search,
            page,
        },
        None,
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
    if !uri.path().starts_with("/admin/") {
        return settings_result(&ctx, "access", &form, result, "account").await;
    }
    let listing = listing_from_form(&form);
    match result {
        Ok(()) => ctx.redirect(&users_path(&listing, "account")),
        Err(error) => {
            users_response(
                &ctx,
                listing,
                Some(&form),
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
                Some(&form),
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
                Some(&form),
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
            users_response(
                &ctx,
                listing,
                Some(&form),
                status,
                Some(public_api_message(&body.0)),
            )
            .await
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
                Some(&form),
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
                Some(&form),
                failure.0,
                Some(public_api_message(&failure.1 .0)),
            )
            .await
        }
    }
}
