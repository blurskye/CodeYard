use crate::backend::{
    api::{
        http::context::PageContext,
        repos::{chrome::Loaded, forms::field},
    },
    db,
};
use axum::{http::StatusCode, response::Response};
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
    let result = if action == "variable-save" {
        let previous = field(form, "previous").to_ascii_uppercase();
        db::variables::save(
            &ctx.state.pool,
            actor.id,
            loaded.repository.id,
            (!previous.is_empty()).then_some(previous.as_str()),
            &name,
            Some(&field(form, "value")),
        )
        .await
    } else {
        db::variables::delete(&ctx.state.pool, actor.id, loaded.repository.id, &name).await
    };
    match result {
        Ok(()) => ctx.redirect(&format!("{}/settings?tab=secrets", loaded.chrome.base())),
        Err(error) => {
            use db::variables::VariableError;
            let (status, message) = match error {
                VariableError::Denied => (
                    StatusCode::FORBIDDEN,
                    "Repository administration is required.".into(),
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
