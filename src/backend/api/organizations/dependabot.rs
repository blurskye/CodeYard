use crate::backend::api::http::context::PageContext;
use crate::backend::api::organizations::organization::Fields;
use crate::backend::api::organizations::INDEX;
use crate::backend::app::AppState;

use axum::{
    extract::{rejection::FormRejection, Form, Path, State},
    http::{HeaderMap, StatusCode, Uri},
    response::Response,
};
use codeyard_db_core::queries::secrets::SecretsError;
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
        Err(_) => return crate::backend::api::organizations::organization::invalid(&ctx),
    };
    if let Err(response) = ctx.check_csrf(&form) {
        return response;
    }
    ctx.query.insert("tab".into(), "dependabot".into());
    if let Err(response) =
        crate::backend::api::organizations::organization::selected(&ctx, actor, org).await
    {
        return response;
    }
    let draft = crate::backend::api::organizations::organization::draft(&form);
    let result = if !crate::backend::api::organizations::organization::bounded(&form) {
        Err(SecretsError::InvalidRepositorySelection)
    } else {
        match crate::backend::api::organizations::organization::field(&form, "action") {
            "dependabot-default" => {
                codeyard_db_security::dependabot::set_repository_access_default(
                    &ctx.state.pool,
                    ctx.database_actor(actor),
                    org,
                    crate::backend::api::organizations::organization::field(&form, "default_level"),
                )
                .await
            }
            action @ ("dependabot-add" | "dependabot-remove") => {
                match crate::backend::api::organizations::organization::id(&form, "repo_id") {
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
        Err(SecretsError::NotFound) => {
            return crate::backend::api::organizations::organization::not_found(&ctx)
        }
        Err(SecretsError::Denied) => {
            return ctx.error(
                StatusCode::FORBIDDEN,
                "This organization operation is not permitted.",
            )
        }
        Err(SecretsError::InvalidRepositorySelection) => StatusCode::UNPROCESSABLE_ENTITY,
        Err(_) => return crate::backend::api::organizations::organization::unavailable(&ctx),
    };
    crate::backend::api::organizations::organization::respond(
        &ctx,
        Some(org),
        status,
        Some("Check the access level and organization repository ID.".into()),
        draft,
    )
    .await
}
