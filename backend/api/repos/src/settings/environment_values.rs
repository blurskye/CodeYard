use axum::http::StatusCode;
use axum::response::Response;
use codeyard_api_http::context::PageContext;
use codeyard_api_http::shared::repos::chrome::Loaded;
use codeyard_api_http::shared::repos::forms::field;
use codeyard_db_actions::environment_values as values;
use codeyard_db_actions::environment_values::Save;
use codeyard_db_actions::environment_values::ValueError;
use codeyard_db_orgs::organization_values::Kind;
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
                ctx.database_actor(actor.id),
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
                ctx.database_actor(actor.id),
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
                ctx.database_actor(actor.id),
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
            codeyard_webui::encode_component(&environment)
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
