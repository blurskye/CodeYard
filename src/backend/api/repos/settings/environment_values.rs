use crate::backend::{
    api::{
        http::context::PageContext,
        repos::{chrome::Loaded, forms::field},
    },
    db::environment_values::{self as values, Kind, Save, ValueError},
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
        return ctx.error(
            StatusCode::UNAUTHORIZED,
            "Sign in to manage environment values.",
        );
    };
    let authority = crate::backend::db::users::accounts::AccountActor::Session {
        user_id: actor.id,
        session: crate::backend::api::http::session::extract_session_id(&ctx.headers).unwrap_or_default(),
    };
    let environment = field(form, "environment");
    let name = field(form, "name");
    let previous = field(form, "previous");
    let value = field(form, "value");
    if action.ends_with("-delete") && field(form, "confirm") != "yes" {
        return ctx.repo_error(
            &loaded.chrome,
            StatusCode::UNPROCESSABLE_ENTITY,
            "Confirm deletion before submitting.",
        );
    }
    let result = match action {
        "environment-create" => {
            values::create(
                &ctx.state.pool,
                authority,
                loaded.repository.id,
                &environment,
            )
            .await
        }
        "environment-secret-save" | "environment-variable-save" => {
            let kind = if action == "environment-secret-save" {
                Kind::Secret
            } else {
                Kind::Variable
            };
            values::save(
                &ctx.state.pool,
                ctx.state.secret_encryptor.as_deref(),
                authority,
                loaded.repository.id,
                &environment,
                &Save {
                    kind,
                    previous: (!previous.is_empty()).then_some(previous.as_str()),
                    name: &name,
                    value: Some(&value),
                },
            )
            .await
            .map(|_| ())
        }
        "environment-secret-delete" | "environment-variable-delete" => {
            let kind = if action == "environment-secret-delete" {
                Kind::Secret
            } else {
                Kind::Variable
            };
            values::delete(
                &ctx.state.pool,
                authority,
                loaded.repository.id,
                &environment,
                kind,
                &name,
            )
            .await
        }
        _ => {
            return ctx.repo_error(
                &loaded.chrome,
                StatusCode::NOT_FOUND,
                "Unknown environment action.",
            )
        }
    };
    match result {
        Ok(()) => ctx.redirect(&format!(
            "{}/settings?tab=secrets&environment={}",
            loaded.chrome.base(),
            crate::webui::encode_component(&environment)
        )),
        Err(error) => {
            let (status, message) = match error {
                ValueError::Denied => (
                    StatusCode::FORBIDDEN,
                    "Repository administration is required.".to_owned(),
                ),
                ValueError::NotFound => (
                    StatusCode::NOT_FOUND,
                    "Environment or value not found.".to_owned(),
                ),
                ValueError::Database(_) | ValueError::Authority(_) | ValueError::Encryption(_) => (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Environment values are unavailable right now.".to_owned(),
                ),
                _ => (StatusCode::UNPROCESSABLE_ENTITY, error.to_string()),
            };
            ctx.repo_error(&loaded.chrome, status, &message)
        }
    }
}
