use crate::organizations::organization::Fields;
use crate::organizations::INDEX;
use axum::extract::rejection::FormRejection;
use axum::extract::Form;
use axum::extract::Path;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::http::Uri;
use axum::response::Response;
use codeyard_api_http::context::PageContext;
use codeyard_app::app::AppState;
use codeyard_database::queries::secrets::SecretsError;
use std::sync::Arc;

pub(super) async fn post(
    State(state): State<Arc<AppState>>,
    Path(org): Path<i64>,
    headers: HeaderMap,
    uri: Uri,
    form: Result<Form<Fields>, FormRejection>,
) -> Response {
    let mut ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    let actor = match ctx.require_user() {
        Ok(user) => user.id,
        Err(response) => return response,
    };
    let form = match form {
        Ok(Form(form)) => form,
        Err(_) => return crate::organizations::organization::invalid(&ctx),
    };
    if let Err(response) = ctx.check_csrf(&form) {
        return response;
    }
    ctx.query.insert("tab".into(), "dependabot".into());
    if let Err(response) = crate::organizations::organization::selected(&ctx, actor, org).await {
        return response;
    }
    let draft = crate::organizations::organization::draft(&form);
    let result = if !crate::organizations::organization::bounded(&form) {
        Err(SecretsError::InvalidRepositorySelection)
    } else {
        match codeyard_api_http::shared::user::responses::field(&form, "action") {
            "dependabot-default" => {
                codeyard_db_security::dependabot::set_repository_access_default(
                    &ctx.state.pool,
                    ctx.database_actor(actor),
                    org,
                    codeyard_api_http::shared::user::responses::field(&form, "default_level"),
                )
                .await
            }
            action @ ("dependabot-add" | "dependabot-remove") => {
                match crate::organizations::organization::id(&form, "repo_id") {
                    Ok(id) => {
                        let ids = [id];
                        let (add, remove): (&[i64], &[i64]) = if action == "dependabot-add" {
                            (&ids, &[])
                        } else {
                            (&[], &ids)
                        };
                        codeyard_db_security::dependabot::update_repository_access(
                            &ctx.state.pool,
                            ctx.database_actor(actor),
                            org,
                            add,
                            remove,
                        )
                        .await
                    }
                    Err(()) => Err(SecretsError::InvalidRepositorySelection),
                }
            }
            _ => Err(SecretsError::InvalidRepositorySelection),
        }
    };
    let status = match result {
        Ok(()) => return ctx.redirect(&format!("{INDEX}/{org}?tab=dependabot")),
        Err(SecretsError::NotFound) => return crate::organizations::organization::not_found(&ctx),
        Err(SecretsError::Denied) => {
            return ctx.error(
                StatusCode::FORBIDDEN,
                "This organization operation is not permitted.",
            )
        }
        Err(SecretsError::InvalidRepositorySelection) => StatusCode::UNPROCESSABLE_ENTITY,
        Err(_) => return crate::organizations::organization::unavailable(&ctx),
    };
    crate::organizations::organization::respond(
        &ctx,
        Some(org),
        status,
        Some("Check the access level and organization repository ID.".into()),
        draft,
    )
    .await
}
