//! Contents API preconditions use blob IDs; publication still leases the entire branch tip.

use axum::http::StatusCode;
use codeyard_git::error::ErrorKind;
use codeyard_git::error::RepoError;
use codeyard_model::github_error::Error;
pub fn store_error(error: RepoError) -> Error {
    if error.is_write_denied() || error.is_protected_branch() {
        return Error::new(StatusCode::FORBIDDEN, "Repository write denied");
    }
    if error.is_response_too_large() {
        return Error::new(
            StatusCode::FORBIDDEN,
            "Object exceeds the response size limit",
        );
    }
    match error.kind {
        ErrorKind::NotFound | ErrorKind::MissingPath => Error::missing(),
        ErrorKind::Conflict => Error::new(StatusCode::CONFLICT, "Reference changed"),
        ErrorKind::Invalid => Error::invalid("Invalid repository operation"),
        ErrorKind::Unavailable => Error::unavailable(()),
    }
}
