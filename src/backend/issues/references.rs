//! Resolve human issue references without exposing private repositories.
use super::{db, OperationError};
pub async fn resolve(
    pool: &codeyard_db_core::DbPool,
    actor_id: i64,
    (owner, repo): (&str, &str),
    reference: &str,
) -> Result<codeyard_db_core::models::Issue, OperationError> {
    let invalid = || OperationError::Invalid {
        code: "invalid_reference",
        message: "Use an issue number or owner/repository#number.".into(),
    };
    let missing = || OperationError::Missing {
        code: "not_found",
        message: "Issue not found.".into(),
    };
    let unavailable = |_| OperationError::Unavailable {
        code: "database_error",
        message: "Issue lookup unavailable.".into(),
    };
    if reference.len() > 1024 || reference.contains('\0') {
        return Err(invalid());
    }
    let (scope, number) = reference.rsplit_once('#').unwrap_or(("", reference));
    let (owner, repo) = if scope.is_empty() {
        (owner, repo)
    } else {
        scope.split_once('/').ok_or_else(invalid)?
    };
    let number = number
        .parse::<i32>()
        .ok()
        .filter(|n| *n > 0)
        .ok_or_else(invalid)?;
    let actor = super::actor(pool, actor_id).await?;
    let repo = db::repositories::records::get_repository(pool, owner, repo)
        .await
        .map_err(unavailable)?
        .ok_or_else(missing)?;
    super::require_read(pool, &actor, &repo)
        .await
        .map_err(|error| match error {
            OperationError::Missing { .. } => missing(),
            other => other,
        })?;
    db::issues::get_issue(pool, repo.id, number)
        .await
        .map_err(unavailable)?
        .ok_or_else(missing)
}
