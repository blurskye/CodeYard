//! Reading and answering the HTML forms of the repository pages.
use crate::backend::api::http::context::PageContext;
use axum::http::StatusCode;
use axum::response::Response;
use std::collections::HashMap;

pub(crate) fn field(form: &HashMap<String, String>, name: &str) -> String {
    form.get(name).cloned().unwrap_or_default()
}

/// The values a repeated form field was sent with, in the order they were sent.
pub(crate) fn repeated(pairs: &[(String, String)], name: &str) -> Vec<String> {
    pairs
        .iter()
        .filter(|(key, _)| key == name)
        .map(|(_, value)| value.clone())
        .collect()
}

/// A native form sends a checked checkbox as "on" and an unchecked one not at all.
pub(crate) fn checkbox(form: &HashMap<String, String>, name: &str) -> bool {
    matches!(form.get(name).map(String::as_str), Some("on" | "true"))
}

/// Sets a page's status, but never turns a renderer or runtime failure into a validation result
/// or a success.
pub(crate) fn with_status(mut response: Response, status: StatusCode) -> Response {
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
pub(crate) fn authorize_post(
    ctx: &PageContext,
    form: &HashMap<String, String>,
) -> Result<(), Response> {
    ctx.check_csrf(form)?;
    ctx.require_user()?;
    Ok(())
}
