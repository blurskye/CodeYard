use crate::backend::db;
use axum::{http::StatusCode, Json};
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct ApiError {
    pub error: String,
    pub message: String,
}

/// A typed authority refusal survives anyhow context; driver details never do.
pub(crate) fn protection_write_error(
    error: anyhow::Error,
) -> (axum::http::StatusCode, axum::Json<ApiError>) {
    if error.is::<codeyard_support::credentials::AccountRestricted>() {
        return ApiError::failure(
            StatusCode::FORBIDDEN,
            "forbidden",
            "A live unrestricted account credential is required",
        );
    }
    use crate::backend::db::repositories::ProtectionWriteRefusal as Refusal;
    use axum::{http::StatusCode, Json};
    let (status, code, message) = match error.downcast_ref::<Refusal>() {
        Some(Refusal::Denied) => (
            StatusCode::FORBIDDEN,
            "forbidden",
            "Repository policy authority is denied",
        ),
        Some(Refusal::NotFound) => (StatusCode::NOT_FOUND, "not_found", "Branch protection not found"),
        Some(Refusal::InvalidStatusMetadata) => (StatusCode::BAD_REQUEST, "validation_error", "Status context must be 1–255 bytes, description at most 1024 bytes and URL at most 2048 bytes, without NUL."),
        Some(Refusal::InvalidChecks) => (StatusCode::BAD_REQUEST, "validation_error", "A rule can require at most 100 checks of 512 bytes each"),
        Some(Refusal::InvalidPushTeams) => (StatusCode::BAD_REQUEST, "validation_error", "Push allowances must name at most 100 teams with repository write access"),
        Some(Refusal::InvalidPushAllowances) => (StatusCode::BAD_REQUEST, "validation_error", "Push allowances must name at most 100 users with repository write access"),
        Some(Refusal::RuleLimit) => (StatusCode::UNPROCESSABLE_ENTITY, "rule_limit", "A repository can have at most 100 protection rules"),
        Some(Refusal::StatusLimit) => (StatusCode::UNPROCESSABLE_ENTITY, "status_limit", "A commit can have at most 1024 external status contexts"),
        Some(Refusal::StatusHistoryLimit) => (StatusCode::UNPROCESSABLE_ENTITY, "status_limit", "A commit can have at most 1000 statuses for one context"),
        Some(Refusal::InvalidPattern) => (
            StatusCode::BAD_REQUEST,
            "validation_error",
            "Use a branch name or a pattern with *, ** or ?",
        ),
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            "unavailable",
            "Repository policy is temporarily unavailable",
        ),
    };
    (status, Json(ApiError::new(code, message)))
}

/// The status of a refused native OIDC settings save: a denial is 403, any other typed refusal
/// is 422, and an untyped failure is 503.
pub(crate) fn oidc_settings_status(error: &anyhow::Error) -> StatusCode {
    match error.downcast_ref::<db::oidc::Refusal>() {
        Some(db::oidc::Refusal::Denied) => StatusCode::FORBIDDEN,
        Some(_) => StatusCode::UNPROCESSABLE_ENTITY,
        None => StatusCode::SERVICE_UNAVAILABLE,
    }
}

/// Current native settings authority, without driver/path disclosure.
pub(crate) fn repository_settings_error(
    error: anyhow::Error,
) -> (axum::http::StatusCode, axum::Json<ApiError>) {
    use crate::backend::db::repositories::RepositorySettingsRefusal as Refusal;
    use axum::{http::StatusCode, Json};
    if error.is::<codeyard_support::credentials::AccountRestricted>() {
        return ApiError::failure(
            StatusCode::FORBIDDEN,
            "forbidden",
            "A live unrestricted credential is required.",
        );
    }
    let (status, code, message) = match error.downcast_ref::<Refusal>() {
        Some(Refusal::Denied) => (
            StatusCode::FORBIDDEN,
            "forbidden",
            "Repository settings authority is denied",
        ),
        Some(Refusal::VisibilityLocked) => (
            StatusCode::FORBIDDEN,
            "visibility_locked",
            "Nonpublic fork history cannot be made public",
        ),
        Some(Refusal::InvalidName) => (
            StatusCode::CONFLICT,
            "invalid_name",
            "Repository name is invalid",
        ),
        Some(Refusal::NameConflict) => (
            StatusCode::CONFLICT,
            "name_reserved",
            "Repository name is already reserved",
        ),
        Some(Refusal::NoMergeMethod) => (
            StatusCode::BAD_REQUEST,
            "validation_error",
            "At least one merge method must stay allowed",
        ),
        Some(Refusal::InvalidTopics) => (
            StatusCode::BAD_REQUEST,
            "validation_error",
            "Topics use lowercase letters, numbers and hyphens, up to 35 characters each, and up to 20 per repository.",
        ),
        Some(Refusal::InvalidWebsite) => (
            StatusCode::BAD_REQUEST,
            "validation_error",
            "Website must be an http or https address of at most 255 characters without spaces.",
        ),
        Some(Refusal::InternalNeedsOrganization) => (
            StatusCode::BAD_REQUEST,
            "validation_error",
            "Only organization repositories can be internal",
        ),
        Some(Refusal::InvalidSquashMessage) => (
            StatusCode::BAD_REQUEST,
            "validation_error",
            "Invalid squash merge commit title or message",
        ),
        None if matches!(error.downcast_ref::<crate::backend::db::secrets::SecretsError>(), Some(crate::backend::db::secrets::SecretsError::TotalTooLarge | crate::backend::db::secrets::SecretsError::TooManySecrets)) => (
            StatusCode::BAD_REQUEST,
            "validation_error",
            "Delivered Actions secrets exceed the CI delivery budget",
        ),
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            "unavailable",
            "Repository settings are temporarily unavailable",
        ),
    };
    (status, Json(ApiError::new(code, message)))
}

/// What a JSON handler returns when it refuses a request.
pub(crate) type ApiFailure = (StatusCode, Json<ApiError>);

impl ApiError {
    pub fn new(error: &str, message: &str) -> Self {
        Self {
            error: error.to_string(),
            message: message.to_string(),
        }
    }

    pub(crate) fn failure(status: StatusCode, code: &str, message: &str) -> ApiFailure {
        (status, Json(Self::new(code, message)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repository_settings_errors_preserve_refusals_not_private_context() {
        use crate::backend::db::repositories::RepositorySettingsRefusal as Refusal;
        use axum::http::StatusCode;
        for (refusal, expected) in [
            (Refusal::Denied, StatusCode::FORBIDDEN),
            (Refusal::VisibilityLocked, StatusCode::FORBIDDEN),
            (Refusal::InvalidName, StatusCode::CONFLICT),
            (Refusal::NameConflict, StatusCode::CONFLICT),
            (Refusal::NoMergeMethod, StatusCode::BAD_REQUEST),
        ] {
            let (status, axum::Json(body)) = repository_settings_error(
                anyhow::Error::from(refusal).context("private password, SQL and storage paths"),
            );
            assert_eq!(status, expected);
            assert!(!serde_json::to_string(&body).unwrap().contains("private"));
        }
        let (status, axum::Json(body)) =
            repository_settings_error(anyhow::anyhow!("private password, SQL and storage paths"));
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        let body = serde_json::to_string(&body).unwrap();
        for forbidden in ["private", "password", "SQL", "storage paths"] {
            assert!(!body.contains(forbidden));
        }
    }

    #[test]
    fn protection_denial_survives_context_without_driver_disclosure() {
        let refusal =
            anyhow::Error::from(crate::backend::db::repositories::ProtectionWriteRefusal::Denied)
                .context("private driver diagnostic");
        let (status, axum::Json(error)) = protection_write_error(refusal);
        assert_eq!(status, axum::http::StatusCode::FORBIDDEN);
        assert_eq!(error.error, "forbidden");
        assert!(!serde_json::to_string(&error).unwrap().contains("private"));
        let (status, axum::Json(error)) = protection_write_error(
            anyhow::Error::from(
                crate::backend::db::repositories::ProtectionWriteRefusal::InvalidPattern,
            )
            .context("private driver diagnostic"),
        );
        assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
        assert_eq!(error.error, "validation_error");
        assert!(!serde_json::to_string(&error).unwrap().contains("private"));
        let (status, axum::Json(error)) = protection_write_error(anyhow::anyhow!(
            "private bound SQL, password and storage paths"
        ));
        assert_eq!(status, axum::http::StatusCode::SERVICE_UNAVAILABLE);
        let body = serde_json::to_string(&error).unwrap();
        for sensitive in ["private", "password", "SQL", "storage paths"] {
            assert!(!body.contains(sensitive));
        }
    }
}

pub(crate) fn discussion_write_error(error: anyhow::Error) -> (StatusCode, Json<ApiError>) {
    use db::discussions::DiscussionWriteRefusal;
    let (status, code, message) = match error.downcast_ref::<DiscussionWriteRefusal>() {
        Some(DiscussionWriteRefusal::Denied) => (
            StatusCode::FORBIDDEN,
            "forbidden",
            "This discussion operation is not permitted",
        ),
        Some(DiscussionWriteRefusal::Locked) => (
            StatusCode::FORBIDDEN,
            "conversation_locked",
            "This conversation is locked.",
        ),
        Some(DiscussionWriteRefusal::InvalidReview) => (
            StatusCode::CONFLICT,
            "conflict",
            "The pull request no longer accepts this review",
        ),
        Some(DiscussionWriteRefusal::RevisionChanged) => (
            StatusCode::CONFLICT,
            "revision_conflict",
            "The pull request revisions changed. Reload the diff before commenting.",
        ),
        Some(DiscussionWriteRefusal::ReviewFull) => (
            StatusCode::BAD_REQUEST,
            "validation_error",
            "A review holds at most 200 comments.",
        ),
        Some(DiscussionWriteRefusal::EmptyReview) => (
            StatusCode::BAD_REQUEST,
            "validation_error",
            "A review body is required unless the review is approved",
        ),
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            "database_error",
            "Discussion storage is unavailable; no success was confirmed",
        ),
    };
    (status, Json(ApiError::new(code, message)))
}

#[cfg(test)]
mod discussion_errors {
    use super::*;
    use crate::backend::api::repos::responses::repo_store_error;

    #[test]
    fn repository_store_errors_do_not_expose_paths_or_credentials() {
        use codeyard_git::error::{ErrorKind, RepoError};
        let (status, Json(body)) = repo_store_error(
            RepoError::new(
                ErrorKind::Unavailable,
                "postgres://user:password@private/secret.git",
            ),
            StatusCode::BAD_REQUEST,
        );
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body.error, "git_error");
        assert_eq!(body.message, "Repository storage is unavailable");
        let (status, Json(body)) = repo_store_error(
            RepoError::storage_node_unavailable(),
            StatusCode::BAD_REQUEST,
        );
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body.message, "storage node unavailable");
        let (status, Json(body)) =
            repo_store_error(RepoError::response_too_large(), StatusCode::BAD_REQUEST);
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(body.error, "response_too_large");
    }

    #[test]
    fn typed_refusals_survive_context_and_driver_details_are_never_disclosed() {
        use db::discussions::DiscussionWriteRefusal;
        for (refusal, status, code, message) in [
            (
                DiscussionWriteRefusal::Denied,
                StatusCode::FORBIDDEN,
                "forbidden",
                "This discussion operation is not permitted",
            ),
            (
                DiscussionWriteRefusal::InvalidReview,
                StatusCode::CONFLICT,
                "conflict",
                "The pull request no longer accepts this review",
            ),
            (
                DiscussionWriteRefusal::ReviewFull,
                StatusCode::BAD_REQUEST,
                "validation_error",
                "A review holds at most 200 comments.",
            ),
        ] {
            let error = anyhow::Error::new(refusal).context("private-query-detail");
            let (actual, Json(body)) = discussion_write_error(error);
            assert_eq!(actual, status);
            assert_eq!(body.error, code);
            assert_eq!(body.message, message);
            assert!(!body.message.contains("private-query-detail"));
        }
        let (status, Json(body)) =
            discussion_write_error(anyhow::anyhow!("credential-bearing-driver-detail"));
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body.error, "database_error");
        assert_eq!(
            body.message,
            "Discussion storage is unavailable; no success was confirmed"
        );
        assert!(!body.message.contains("credential-bearing-driver-detail"));
    }
}
