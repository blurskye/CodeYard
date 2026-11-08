use axum::http::StatusCode;
use axum::response::Response;
use codeyard_api_http::context::PageContext;
use codeyard_api_http::shared::repos::chrome::Loaded;
use codeyard_api_http::shared::repos::forms::field;
use std::collections::HashMap;

pub(crate) async fn form(
    ctx: &PageContext,
    loaded: &Loaded,
    action: &str,
    form: &HashMap<String, String>,
) -> Response {
    let Some(actor) = ctx.user.as_ref() else {
        return ctx.error(StatusCode::UNAUTHORIZED, "Sign in to manage variables.");
    };
    let name = field(form, "name").to_ascii_uppercase();
    let session = codeyard_api_http::session::extract_session_id(&ctx.headers).unwrap_or_default();
    let credential =
        codeyard_database::queries::users::accounts::AccountCredential::Session(&session);
    let result = if action == "variable-save" {
        let previous = field(form, "previous").to_ascii_uppercase();
        codeyard_db_actions::variables::save(
            &ctx.state.pool,
            actor.id,
            &credential,
            loaded.repository.id,
            (!previous.is_empty()).then_some(previous.as_str()),
            &name,
            Some(&field(form, "value")),
        )
        .await
    } else {
        codeyard_db_actions::variables::delete(
            &ctx.state.pool,
            actor.id,
            &credential,
            loaded.repository.id,
            &name,
        )
        .await
    };
    match result {
        Ok(()) => ctx.redirect(&format!("{}/settings?tab=secrets", loaded.chrome.base())),
        Err(error) => {
            use codeyard_db_actions::variables::VariableError;
            let (status, message) = match error {
                VariableError::Denied => (
                    StatusCode::FORBIDDEN,
                    "Repository write access is required.".into(),
                ),
                VariableError::NotFound => (StatusCode::NOT_FOUND, "Variable not found.".into()),
                VariableError::Database(_) | VariableError::Authority(_) => (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Variables are unavailable right now.".into(),
                ),
                _ => (StatusCode::BAD_REQUEST, error.to_string()),
            };
            ctx.repo_error(&loaded.chrome, status, &message)
        }
    }
}
