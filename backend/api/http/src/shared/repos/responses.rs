use crate::error::ApiError;
use axum::http::StatusCode;
use axum::Json;
use chrono::TimeZone;
use chrono::Utc;
pub async fn repository_fork_counts(
    pool: &codeyard_database::DbPool,
    headers: Option<&axum::http::HeaderMap>,
    ids: &[i64],
) -> Result<std::collections::HashMap<i64, i32>, (StatusCode, Json<ApiError>)> {
    let (viewer, session) = match headers {
        Some(headers) => (
            crate::session::get_optional_user(pool, headers)
                .await?
                .map(|user| user.id),
            Some(crate::session::extract_session_id(headers).unwrap_or_default()),
        ),
        None => (None, None),
    };
    let mut counts = std::collections::HashMap::new();
    for ids in ids.chunks(100) {
        counts.extend(
            codeyard_db_repos::repositories::discovery::visible_fork_counts(
                pool,
                viewer,
                session.as_deref(),
                ids,
            )
            .await
            .map_err(|_| repository_unavailable())?,
        );
    }
    Ok(counts)
}

/// A Unix timestamp as RFC 3339, or `None` when it is out of range.
pub fn rfc3339(timestamp: i64) -> Option<String> {
    Utc.timestamp_opt(timestamp, 0)
        .single()
        .map(|date| date.to_rfc3339())
}

/// The answer when the database cannot be reached while handling a repository.
pub fn repository_unavailable() -> (StatusCode, Json<ApiError>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(ApiError::new(
            "database_error",
            "Repository operation is unavailable",
        )),
    )
}
