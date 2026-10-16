use crate::backend::api::context::PageContext;
use crate::backend::api::error::ApiError;
use crate::backend::db;
use axum::http::StatusCode;
use axum::response::Response;
use axum::Json;
use serde::Serialize;
use std::collections::HashMap;
use uuid::Uuid;

fn avatar_url(avatar_file_id: Option<Uuid>) -> Option<String> {
    avatar_file_id.map(|id| format!("/api/files/{}/download", id))
}

#[derive(Serialize, Clone)]
pub struct PublicUserResponse {
    pub id: i64,
    pub username: String,
    pub avatar_url: Option<String>,
    pub display_name: Option<String>,
    pub bio: Option<String>,
    pub location: Option<String>,
    pub website: Option<String>,
    pub is_admin: bool,
    pub created_at: String,
}

impl PublicUserResponse {
    pub fn from_display(author: &db::users::accounts::AuthorDisplay) -> Self {
        Self {
            id: author.id,
            username: author.username.clone(),
            avatar_url: avatar_url(author.avatar_file_id),
            display_name: author.display_name.clone(),
            bio: author.bio.clone(),
            location: author.location.clone(),
            website: author.website.clone(),
            is_admin: author.is_admin,
            created_at: author.created_at.to_rfc3339(),
        }
    }
}

impl From<db::models::User> for PublicUserResponse {
    fn from(user: db::models::User) -> Self {
        Self {
            id: user.id,
            username: user.username.clone(),
            avatar_url: avatar_url(user.avatar_file_id),
            display_name: user.display_name,
            bio: user.bio,
            location: user.location,
            website: user.website,
            is_admin: user.is_admin,
            created_at: user.created_at.to_rfc3339(),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct AuthUserResponse {
    pub id: i64,
    pub username: String,
    pub email: Option<String>,
    pub avatar_url: Option<String>,
    pub display_name: Option<String>,
    pub bio: Option<String>,
    pub location: Option<String>,
    pub website: Option<String>,
    pub is_admin: bool,
    pub is_banned: bool,
    pub must_change_password: bool,
    pub created_at: String,
}

impl From<db::models::User> for AuthUserResponse {
    fn from(user: db::models::User) -> Self {
        Self {
            id: user.id,
            username: user.username.clone(),
            email: user.email,
            avatar_url: avatar_url(user.avatar_file_id),
            display_name: user.display_name,
            bio: user.bio,
            location: user.location,
            website: user.website,
            is_admin: user.is_admin,
            is_banned: user.is_banned,
            must_change_password: user.must_change_password,
            created_at: user.created_at.to_rfc3339(),
        }
    }
}

pub(crate) type Fields = HashMap<String, String>;
pub(crate) type ApiFailure = (StatusCode, Json<ApiError>);

pub(crate) fn field<'a>(form: &'a Fields, name: &str) -> &'a str {
    form.get(name).map(String::as_str).unwrap_or("")
}
pub(crate) fn optional_field(form: &Fields, name: &str) -> Option<String> {
    let value = field(form, name).trim();
    if value.is_empty() {
        None
    } else {
        Some(value.to_owned())
    }
}
pub(crate) fn within_bounds(form: &Fields) -> bool {
    form.len() <= 24
        && form.iter().all(|(key, value)| {
            key.len() <= 64
                && value.len()
                    <= match key.as_str() {
                        // A sign-in name may be an email address or a long Unicode username.
                        // Identity checks the real limits.
                        "username" => 1024,
                        "display_name" | "name" => 100,
                        "email" => 320,
                        "location" => 200,
                        "website" | "redirect" => 2048,
                        "public_key" => 16384,
                        "bio" => 4000,
                        _ => 4096,
                    }
        })
}

/// A redirect is always a local absolute path, including after browser decoding.
pub(crate) fn safe_redirect(value: Option<&str>) -> String {
    let value = value.unwrap_or("/");
    if value.len() > 2048
        || !value.starts_with('/')
        || value.starts_with("//")
        || value.chars().any(|c| c.is_control() || c == '\\')
    {
        return "/".into();
    }
    let lower = value.to_ascii_lowercase();
    if lower.starts_with("/%2f")
        || lower.starts_with("/%5c")
        || ["%0a", "%0d", "%00", "%5c", "%25"]
            .iter()
            .any(|pattern| lower.contains(pattern))
    {
        return "/".into();
    }
    value.to_owned()
}

pub(crate) fn public_api_message(error: &ApiError) -> &'static str {
    match error.message.as_str() {
        "Username must be at least 3 bytes and at most 255 characters" => "Username must be at least 3 bytes and at most 255 characters.",
        "Username can only contain letters, numbers, hyphens, and underscores" => "Username can only contain letters, numbers, hyphens, and underscores.",
        "That username is reserved" => "That username is reserved.",
        "Password must be between 8 and 1024 bytes" => "Password must be between 8 and 1024 bytes.",
        "Password exceeds supported bounds" => "Password must be 1024 bytes or fewer.",
        "Email exceeds supported bounds" => "Email must be 255 bytes or fewer.",
        "Credentials exceed supported bounds" => "Username or password exceeds the supported length.",
        "Setup input exceeds supported bounds" => "Setup input exceeds the supported length.",
        "Account storage is temporarily unavailable" => "The account service is temporarily unavailable. Try again shortly.",
        "Session storage is temporarily unavailable" => "Session storage is temporarily unavailable. Try again shortly.",
        "Authentication is temporarily unavailable" => "Authentication is temporarily unavailable. Try again shortly.",
        "Session storage is temporarily unavailable; completed setup or registration remains committed" => "Session storage is temporarily unavailable. Your account was saved; sign in to continue.",
        "Could not issue session; sign out of an active session or retry signing in" => "Could not start a session. Sign out of an active session or try signing in again.",
        "Invalid username or password" => "Invalid username or password.",
        "Account is banned" => "Account is banned.",
        "Invalid admin setup secret" => "Invalid admin setup secret.",
        "Node mode must be either 'public' or 'private'" => "Choose public or private node access.",
        "Node setup has already been completed; sign in with the account created by setup" => "Node setup is already complete, so sign in with the account created during setup.",
        "Existing accounts prevent setup; recovery requires the sole active administrator's original credentials" => "Existing accounts prevent setup. Recovery requires the sole active administrator's original credentials.",
        "Account or pending request already exists" => "An account or pending request already uses this username or email.",
        "Account identity already exists" => "An account already uses this username or email.",
        "Account credentials changed; sign in again" => "Account credentials changed. Sign in again.",
        "Admin user creation is available only in private node mode" => "Managed accounts can be created only on a private node.",
        "Signup request review is available only in private node mode" => "Signup requests can be reviewed only on a private node.",
        "Use a name of 1-100 characters, scope read or write, and a future expiry" => "Use a name of 1–100 characters, scope read or write, and a future expiry.",
        "Token not found" => "Token not found or not owned by you.",
        "Key name must be 1-100 characters" => "Key name must be 1–100 characters.",
        "Invalid SSH public key format" => "Invalid SSH public key format.",
        "This SSH public key is already registered" => "This SSH public key is already registered.",
        "SSH key enrollment is temporarily unavailable" => "SSH key enrollment is temporarily unavailable.",
        "SSH key not found" => "SSH key not found or not owned by you.",
        "Cannot follow yourself" => "You cannot follow yourself.",
        "Cannot unfollow yourself" => "You cannot unfollow yourself.",
        "Follow request completed, but updated counts are temporarily unavailable" => "Follow request completed, but updated counts are temporarily unavailable. Reload before retrying.",
        "Follow storage is temporarily unavailable" => "Follow storage is temporarily unavailable.",
        "Token storage is unavailable" => "Token storage is temporarily unavailable.",
        "Profile fields must not contain NUL characters" => "Profile fields must not contain NUL characters.",
        "Profile storage is temporarily unavailable" => "Profile storage is temporarily unavailable.",
        "One or more profile fields exceed the supported length" => "One or more profile fields exceed the supported length.",
        "You cannot ban your own account" => "You cannot ban your own account.",
        "Cannot ban the last active admin" => "Cannot ban the last active administrator.",
        "User not found" => "User not found.",
        "Action must be either 'approve' or 'reject'" => "Choose approve or reject.",
        _ => match error.error.as_str() {
            "rate_limited" => "Too many attempts. Try again later.",
            "invalid_session_cookie" => "Clear malformed or duplicate session cookies before signing in.",
            "password_change_required" => "Change your temporary password before continuing.",
            "database_error" | "session_error" => "The account service is unavailable. The action may be incomplete; check its state before retrying.",
            "setup_required" => "Complete node setup before signing in or creating accounts.",
            "unauthorized" => "Sign in to perform this action.",
            "forbidden" => "You do not have permission to perform this action.",
            "not_found" => "The requested account or item was not found. Check the address and try again.",
            "conflict" => "The account or request has changed. Reload and check its current state.",
            _ => "The account action could not be completed. Check the submitted fields and reload its current state before retrying.",
        }
    }
}

pub(crate) fn api_error(ctx: &PageContext, failure: ApiFailure) -> Response {
    ctx.error(failure.0, public_api_message(&failure.1 .0))
}

pub(crate) fn account_permission(
    authenticated: bool,
    banned: bool,
    admin: bool,
    require_admin: bool,
) -> Result<(), StatusCode> {
    if !authenticated {
        return Err(StatusCode::UNAUTHORIZED);
    }
    if banned || (require_admin && !admin) {
        return Err(StatusCode::FORBIDDEN);
    }
    Ok(())
}
#[expect(
    clippy::result_large_err,
    reason = "HTML adapters preserve complete Axum responses under the PageContext contract."
)]
pub(crate) fn require_account(
    ctx: &PageContext,
    admin: bool,
) -> Result<&db::models::User, Response> {
    let user = ctx.require_user()?;
    account_permission(true, user.is_banned, user.is_admin, admin).map_err(|status| {
        let message = if admin {
            "Only site administrators can open this page."
        } else {
            "You do not have permission to perform this action."
        };
        ctx.error(status, message)
    })?;
    Ok(user)
}
#[expect(
    clippy::result_large_err,
    reason = "HTML adapters preserve complete Axum responses under the PageContext contract."
)]
pub(crate) fn mutation_guard(
    ctx: &PageContext,
    form: &Fields,
    admin: bool,
) -> Result<(), Response> {
    ctx.check_csrf(form)?;
    require_account(ctx, admin)?;
    Ok(())
}

#[derive(Debug, Serialize)]
pub struct AuthResponse {
    pub success: bool,
    pub user: Option<AuthUserResponse>,
    pub message: Option<String>,
}
