//! Reading and answering the HTML forms of the repository pages.
use crate::context::PageContext;
use axum::http::StatusCode;
use axum::response::Response;
use std::collections::HashMap;
pub fn field(form: &HashMap<String, String>, name: &str) -> String {
    crate::shared::user::responses::field(form, name).to_owned()
}

/// Sets a page's status, but never turns a renderer or runtime failure into a validation result
/// or a success.
pub fn with_status(mut response: Response, status: StatusCode) -> Response {
    if response.status() == StatusCode::OK {
        *response.status_mut() = status;
    }
    response
}

/// A form post needs a valid CSRF token and a signed-in user.
#[expect(
    clippy::result_large_err,
    reason = "HTML adapters preserve complete Axum responses under the PageContext contract."
)]
pub fn authorize_post(ctx: &PageContext, form: &HashMap<String, String>) -> Result<(), Response> {
    ctx.check_csrf(form)?;
    ctx.require_user()?;
    Ok(())
}
