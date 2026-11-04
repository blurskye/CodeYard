//! HTTP projection of concrete identity operation failures. No driver text escapes.

use crate::error::ApiError;
use crate::error::ApiFailure;
use axum::http::StatusCode;
use codeyard_identity::sessions::AccountAuthorityError;
pub(crate) fn session_failure() -> ApiFailure {
    ApiError::failure(
        StatusCode::SERVICE_UNAVAILABLE,
        "session_unavailable",
        "Session storage is temporarily unavailable",
    )
}

pub fn admin_authority_failure(error: AccountAuthorityError) -> ApiFailure {
    match error {
        AccountAuthorityError::NotAuthenticated | AccountAuthorityError::SessionExpired => {
            ApiError::failure(
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "Session expired or not authenticated",
            )
        }
        AccountAuthorityError::SessionUnavailable => session_failure(),
        AccountAuthorityError::PasswordChangeRequired => ApiError::failure(
            StatusCode::PRECONDITION_REQUIRED,
            "password_change_required",
            "Change your temporary password before accessing account resources",
        ),
        AccountAuthorityError::AdminRequired => {
            ApiError::failure(StatusCode::FORBIDDEN, "forbidden", "Admin access required")
        }
    }
}
