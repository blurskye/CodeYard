//! HTTP projection of concrete identity operation failures. No driver text escapes.

use crate::backend::api::http::error::ApiError;
use crate::backend::identity::accounts::{
    BanError, EmailChangeError, ManagedAccountError, PasswordChangeError, RoleError,
    SignupReviewError,
};
use crate::backend::identity::admission::AdmissionError;
use crate::backend::identity::credentials::AccountInputError;
use crate::backend::identity::login::LoginError;
use crate::backend::identity::passwords::PasswordError;
use crate::backend::identity::registration::RegistrationError;
use crate::backend::identity::sessions::{AccountAuthorityError, SessionIssueError};
use crate::backend::identity::setup::SetupError;
use crate::backend::identity::two_factor::TwoFactorError;
use axum::http::StatusCode;
use axum::Json;

type Failure = (StatusCode, Json<ApiError>);

fn failure(status: StatusCode, code: &str, message: &str) -> Failure {
    (status, Json(ApiError::new(code, message)))
}

pub(crate) fn database_failure() -> Failure {
    failure(
        StatusCode::SERVICE_UNAVAILABLE,
        "database_unavailable",
        "Account storage is temporarily unavailable",
    )
}

pub(crate) fn session_failure() -> Failure {
    failure(
        StatusCode::SERVICE_UNAVAILABLE,
        "session_unavailable",
        "Session storage is temporarily unavailable",
    )
}

pub(crate) fn admission_failure(error: AdmissionError) -> Failure {
    match error {
        AdmissionError::Limited => failure(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limited",
            "Too many attempts. Try again later.",
        ),
        AdmissionError::Unavailable => authentication_unavailable(),
    }
}

fn authentication_unavailable() -> Failure {
    failure(
        StatusCode::SERVICE_UNAVAILABLE,
        "authentication_unavailable",
        "Authentication is temporarily unavailable",
    )
}

fn password_failure(error: PasswordError) -> Failure {
    match error {
        PasswordError::InvalidInput => failure(
            StatusCode::BAD_REQUEST,
            "validation_error",
            "Password exceeds supported bounds",
        ),
        PasswordError::Busy | PasswordError::Failed => authentication_unavailable(),
    }
}

fn input_failure(error: AccountInputError) -> Failure {
    let message = match error {
        AccountInputError::UsernameBounds => {
            "Username must be at least 3 bytes and at most 255 characters"
        }
        AccountInputError::UsernameCharacters => {
            "Username can only contain letters, numbers, hyphens, and underscores"
        }
        AccountInputError::UsernameReserved => "That username is reserved",
        AccountInputError::PasswordBounds => "Password must be between 8 and 1024 bytes",
        AccountInputError::EmailBounds => "Email exceeds supported bounds",
    };
    failure(StatusCode::BAD_REQUEST, "validation_error", message)
}

fn invalid_credentials() -> Failure {
    failure(
        StatusCode::UNAUTHORIZED,
        "unauthorized",
        "Invalid username or password",
    )
}

pub(crate) fn session_issue_failure(error: SessionIssueError) -> Failure {
    match error {
        SessionIssueError::InvalidInitiator => failure(StatusCode::BAD_REQUEST, "invalid_session_cookie", "Clear malformed or duplicate session cookies before signing in"),
        SessionIssueError::Unavailable => failure(StatusCode::SERVICE_UNAVAILABLE, "session_unavailable", "Session storage is temporarily unavailable; completed setup or registration remains committed"),
        SessionIssueError::Denied => failure(StatusCode::CONFLICT, "session_error", "Could not issue session; sign out of an active session or retry signing in"),
    }
}

pub(crate) fn login_failure(error: LoginError) -> Failure {
    match error {
        LoginError::CredentialsBounds => failure(
            StatusCode::BAD_REQUEST,
            "validation_error",
            "Credentials exceed supported bounds",
        ),
        LoginError::SetupRequired => failure(
            StatusCode::PRECONDITION_REQUIRED,
            "setup_required",
            "Node setup is required before signing in",
        ),
        LoginError::AccountUnavailable => database_failure(),
        LoginError::Admission(error) => admission_failure(error),
        LoginError::Password(error) => password_failure(error),
        LoginError::InvalidCredentials => invalid_credentials(),
        LoginError::Banned => failure(StatusCode::FORBIDDEN, "forbidden", "Account is banned"),
        LoginError::Session(error) => session_issue_failure(error),
        LoginError::TwoFactorUnavailable => failure(
            StatusCode::SERVICE_UNAVAILABLE,
            "two_factor_unavailable",
            "Two-factor sign-in is unavailable on this node",
        ),
        LoginError::PendingLoginExpired => failure(
            StatusCode::UNAUTHORIZED,
            "two_factor_expired",
            "Your sign-in expired. Sign in again.",
        ),
        LoginError::InvalidTwoFactorCode => failure(
            StatusCode::UNAUTHORIZED,
            "invalid_two_factor_code",
            "That code does not match. Check your app and try again.",
        ),
    }
}

/// The JSON API does not complete two-factor sign-in, so a password is not enough for it.
pub(crate) fn two_factor_required_failure() -> Failure {
    failure(
        StatusCode::FORBIDDEN,
        "two_factor_required",
        "This account signs in with two-factor authentication on the web. Use an access token for the API and Git.",
    )
}

pub(crate) fn two_factor_failure(error: TwoFactorError) -> Failure {
    match error {
        TwoFactorError::KeyUnavailable => failure(
            StatusCode::SERVICE_UNAVAILABLE,
            "two_factor_unavailable",
            "Two-factor authentication needs the secrets master key on this node",
        ),
        TwoFactorError::Unavailable => database_failure(),
        TwoFactorError::Admission(error) => admission_failure(error),
        TwoFactorError::Password(error) => password_failure(error),
        TwoFactorError::InvalidPassword => failure(
            StatusCode::UNAUTHORIZED,
            "invalid_password",
            "Current password is incorrect",
        ),
        TwoFactorError::PasswordRequired => failure(
            StatusCode::FORBIDDEN,
            "password_required",
            "Two-factor enrollment requires an account password",
        ),
        TwoFactorError::InvalidCode => failure(
            StatusCode::BAD_REQUEST,
            "invalid_two_factor_code",
            "That code does not match. Check your app and try again.",
        ),
        TwoFactorError::AlreadyEnabled => failure(
            StatusCode::CONFLICT,
            "conflict",
            "Two-factor authentication is already on",
        ),
        TwoFactorError::NotEnabled => failure(
            StatusCode::CONFLICT,
            "conflict",
            "Two-factor authentication is not on",
        ),
        TwoFactorError::SetupExpired => failure(
            StatusCode::BAD_REQUEST,
            "validation_error",
            "This setup expired. Start again.",
        ),
        TwoFactorError::Authority(error) => admin_authority_failure(error),
    }
}

pub(crate) fn registration_failure(error: RegistrationError) -> Failure {
    match error {
        RegistrationError::Input(error) => input_failure(error),
        RegistrationError::Admission(error) => admission_failure(error),
        RegistrationError::Password(error) => password_failure(error),
        RegistrationError::AccountUnavailable => database_failure(),
        RegistrationError::SetupRequired => failure(
            StatusCode::PRECONDITION_REQUIRED,
            "setup_required",
            "Node setup is required before creating accounts",
        ),
        RegistrationError::Conflict => failure(
            StatusCode::CONFLICT,
            "conflict",
            "Account or pending request already exists",
        ),
        RegistrationError::Denied => failure(
            StatusCode::FORBIDDEN,
            "forbidden",
            "Account creation denied",
        ),
        RegistrationError::Session(error) => session_issue_failure(error),
    }
}

pub(crate) fn setup_failure(error: SetupError) -> Failure {
    match error {
        SetupError::Input(error) => input_failure(error),
        SetupError::InputBounds => failure(StatusCode::BAD_REQUEST, "validation_error", "Setup input exceeds supported bounds"),
        SetupError::InvalidNodeMode => failure(StatusCode::BAD_REQUEST, "validation_error", "Node mode must be either 'public' or 'private'"),
        SetupError::InvalidAdminSecret => failure(StatusCode::FORBIDDEN, "forbidden", "Invalid admin setup secret"),
        SetupError::Admission(error) => admission_failure(error),
        SetupError::Password(error) => password_failure(error),
        SetupError::AccountUnavailable => database_failure(),
        SetupError::AlreadyCompleted => failure(StatusCode::CONFLICT, "conflict", "Node setup has already been completed; sign in with the account created by setup"),
        SetupError::ExistingAccounts => failure(StatusCode::CONFLICT, "conflict", "Existing accounts prevent setup; recovery requires the sole active administrator's original credentials"),
        SetupError::Session(error) => session_issue_failure(error),
    }
}

fn admin_authority_failure(error: AccountAuthorityError) -> Failure {
    match error {
        AccountAuthorityError::NotAuthenticated | AccountAuthorityError::SessionExpired => failure(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "Session expired or not authenticated",
        ),
        AccountAuthorityError::SessionUnavailable => session_failure(),
        AccountAuthorityError::PasswordChangeRequired => failure(
            StatusCode::PRECONDITION_REQUIRED,
            "password_change_required",
            "Change your temporary password before accessing account resources",
        ),
        AccountAuthorityError::AdminRequired => {
            failure(StatusCode::FORBIDDEN, "forbidden", "Admin access required")
        }
    }
}

pub(crate) fn password_change_failure(error: PasswordChangeError) -> Failure {
    match error {
        PasswordChangeError::Authority(AccountAuthorityError::NotAuthenticated) => failure(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "Not authenticated",
        ),
        PasswordChangeError::Authority(AccountAuthorityError::SessionExpired) => failure(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "Session expired or revoked",
        ),
        PasswordChangeError::Authority(error) => admin_authority_failure(error),
        PasswordChangeError::Input(error) => input_failure(error),
        PasswordChangeError::Admission(error) => admission_failure(error),
        PasswordChangeError::Password(error) => password_failure(error),
        PasswordChangeError::InvalidCredentials => invalid_credentials(),
        PasswordChangeError::UnchangedPassword => failure(
            StatusCode::BAD_REQUEST,
            "validation_error",
            "Choose a password different from the temporary password",
        ),
        PasswordChangeError::AccountUnavailable => database_failure(),
        PasswordChangeError::CredentialsChanged => failure(
            StatusCode::CONFLICT,
            "conflict",
            "Account credentials changed; sign in again",
        ),
    }
}

pub(crate) fn managed_account_failure(error: ManagedAccountError) -> Failure {
    match error {
        ManagedAccountError::Authority(error) => admin_authority_failure(error),
        ManagedAccountError::PrivateNodeRequired => failure(
            StatusCode::FORBIDDEN,
            "forbidden",
            "Admin user creation is available only in private node mode",
        ),
        ManagedAccountError::Input(error) => input_failure(error),
        ManagedAccountError::Admission(error) => admission_failure(error),
        ManagedAccountError::Password(error) => password_failure(error),
        ManagedAccountError::AccountUnavailable => database_failure(),
        ManagedAccountError::Conflict => failure(
            StatusCode::CONFLICT,
            "conflict",
            "Account identity already exists",
        ),
        ManagedAccountError::Denied => failure(
            StatusCode::FORBIDDEN,
            "forbidden",
            "Account creation is no longer authorized",
        ),
    }
}

pub(crate) fn signup_review_failure(error: SignupReviewError) -> Failure {
    match error {
        SignupReviewError::Authority(error) => admin_authority_failure(error),
        SignupReviewError::PrivateNodeRequired => failure(
            StatusCode::FORBIDDEN,
            "forbidden",
            "Signup request review is available only in private node mode",
        ),
        SignupReviewError::InvalidDecision => failure(
            StatusCode::BAD_REQUEST,
            "validation_error",
            "Action must be either 'approve' or 'reject'",
        ),
        SignupReviewError::Denied => failure(
            StatusCode::FORBIDDEN,
            "forbidden",
            "Current administrator authority required",
        ),
        SignupReviewError::Missing => failure(
            StatusCode::NOT_FOUND,
            "not_found",
            "Signup request not found",
        ),
        SignupReviewError::Processed => failure(
            StatusCode::CONFLICT,
            "conflict",
            "Signup request already processed",
        ),
        SignupReviewError::Conflict => failure(
            StatusCode::CONFLICT, "conflict", "Account identity is already in use; reject this request and ask the applicant to register again",
        ),
        SignupReviewError::AccountUnavailable => database_failure(),
    }
}

pub(crate) fn ban_failure(error: BanError) -> Failure {
    match error {
        BanError::Authority(error) => admin_authority_failure(error),
        BanError::AccountUnavailable => database_failure(),
        BanError::Denied => failure(
            StatusCode::FORBIDDEN,
            "forbidden",
            "Current administrator authority required",
        ),
        BanError::Missing => failure(StatusCode::NOT_FOUND, "not_found", "User not found"),
        BanError::SelfBan => failure(
            StatusCode::BAD_REQUEST,
            "validation_error",
            "You cannot ban your own account",
        ),
        BanError::LastAdmin => failure(
            StatusCode::CONFLICT,
            "conflict",
            "Cannot ban the last active admin",
        ),
    }
}

pub(crate) fn email_failure(error: EmailChangeError) -> Failure {
    match error {
        EmailChangeError::Authority(error) => {
            password_change_failure(PasswordChangeError::Authority(error))
        }
        EmailChangeError::InvalidEmail => failure(
            StatusCode::BAD_REQUEST,
            "validation_error",
            "Enter a valid email address",
        ),
        EmailChangeError::Admission(error) => admission_failure(error),
        EmailChangeError::Password(error) => password_failure(error),
        EmailChangeError::InvalidCredentials => failure(
            StatusCode::BAD_REQUEST,
            "invalid_credentials",
            "Current password is incorrect",
        ),
        EmailChangeError::Taken => failure(
            StatusCode::CONFLICT,
            "conflict",
            "That email address is already used by another account",
        ),
        EmailChangeError::AccountUnavailable => database_failure(),
    }
}

pub(crate) fn role_failure(error: RoleError) -> Failure {
    match error {
        RoleError::Authority(error) => admin_authority_failure(error),
        RoleError::AccountUnavailable => database_failure(),
        RoleError::Denied => failure(
            StatusCode::FORBIDDEN,
            "forbidden",
            "Current administrator authority required",
        ),
        RoleError::Missing => failure(StatusCode::NOT_FOUND, "not_found", "User not found"),
        RoleError::SelfChange => failure(
            StatusCode::BAD_REQUEST,
            "validation_error",
            "You cannot change your own administrator role",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deliberate_identity_errors_keep_their_http_meaning() {
        let limited = login_failure(LoginError::Admission(AdmissionError::Limited));
        assert_eq!(limited.0, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(limited.1 .0.error, "rate_limited");
        let committed = setup_failure(SetupError::Session(SessionIssueError::Unavailable));
        assert_eq!(committed.0, StatusCode::SERVICE_UNAVAILABLE);
        assert!(committed.1 .0.message.contains("remains committed"));
        assert_eq!(ban_failure(BanError::LastAdmin).0, StatusCode::CONFLICT);
        assert_eq!(
            signup_review_failure(SignupReviewError::Missing).0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            managed_account_failure(ManagedAccountError::Authority(
                AccountAuthorityError::PasswordChangeRequired
            ))
            .0,
            StatusCode::PRECONDITION_REQUIRED
        );
    }

    #[test]
    fn every_identity_outcome_has_a_fixed_public_message() {
        use crate::backend::api::user::responses::public_api_message;
        let failures = [
            login_failure(LoginError::CredentialsBounds),
            login_failure(LoginError::Admission(AdmissionError::Limited)),
            login_failure(LoginError::Admission(AdmissionError::Unavailable)),
            login_failure(LoginError::Password(PasswordError::InvalidInput)),
            login_failure(LoginError::InvalidCredentials),
            login_failure(LoginError::Banned),
            login_failure(LoginError::AccountUnavailable),
            login_failure(LoginError::Session(SessionIssueError::Denied)),
            registration_failure(RegistrationError::Input(AccountInputError::UsernameBounds)),
            registration_failure(RegistrationError::Input(
                AccountInputError::UsernameCharacters,
            )),
            registration_failure(RegistrationError::Input(
                AccountInputError::UsernameReserved,
            )),
            registration_failure(RegistrationError::Input(AccountInputError::PasswordBounds)),
            registration_failure(RegistrationError::Input(AccountInputError::EmailBounds)),
            registration_failure(RegistrationError::Conflict),
            setup_failure(SetupError::InputBounds),
            setup_failure(SetupError::InvalidNodeMode),
            setup_failure(SetupError::InvalidAdminSecret),
            setup_failure(SetupError::AlreadyCompleted),
            setup_failure(SetupError::ExistingAccounts),
            setup_failure(SetupError::Session(SessionIssueError::Unavailable)),
            password_change_failure(PasswordChangeError::CredentialsChanged),
            managed_account_failure(ManagedAccountError::Conflict),
        ];
        for (_, Json(error)) in failures {
            assert!(
                !public_api_message(&error)
                    .starts_with("The account action could not be completed"),
                "{}: {}",
                error.error,
                error.message
            );
        }
        let limited = login_failure(LoginError::Admission(AdmissionError::Limited));
        assert_eq!(
            public_api_message(&limited.1 .0),
            "Too many attempts. Try again later."
        );
    }
}
