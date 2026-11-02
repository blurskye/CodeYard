use super::{
    organization::{self, Fields},
    INDEX,
};
use crate::backend::{
    api::http::context::PageContext,
    app::AppState,
    db::{self, secrets::SecretsError},
};
use axum::{
    extract::{rejection::FormRejection, Form, Path, State},
    http::{HeaderMap, StatusCode, Uri},
    response::Response,
};
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
        Err(_) => return organization::invalid(&ctx),
    };
    if let Err(response) = ctx.check_csrf(&form) {
        return response;
    }
    ctx.query.insert("tab".into(), "dependabot".into());
    if let Err(response) = organization::selected(&ctx, actor, org).await {
        return response;
    }
    let draft = organization::draft(&form);
    let result = if !organization::bounded(&form) {
        Err(SecretsError::InvalidRepositorySelection)
    } else {
        match organization::field(&form, "action") {
            "dependabot-default" => {
                db::dependabot::set_repository_access_default(
                    &ctx.state.pool,
                    actor,
                    org,
                    organization::field(&form, "default_level"),
                )
                .await
            }
            action @ ("dependabot-add" | "dependabot-remove") => {
                match organization::id(&form, "repo_id") {
                    Ok(id) => {
                        let ids = [id];
                        let (add, remove): (&[i64], &[i64]) = if action == "dependabot-add" {
                            (&ids, &[])
                        } else {
                            (&[], &ids)
                        };
                        db::dependabot::update_repository_access(
                            &ctx.state.pool,
                            actor,
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
        Err(SecretsError::NotFound) => return organization::not_found(&ctx),
        Err(SecretsError::Denied) => {
            return ctx.error(
                StatusCode::FORBIDDEN,
                "This organization operation is not permitted.",
            )
        }
        Err(SecretsError::InvalidRepositorySelection) => StatusCode::UNPROCESSABLE_ENTITY,
        Err(_) => return organization::unavailable(&ctx),
    };
    organization::respond(
        &ctx,
        Some(org),
        status,
        Some("Check the access level and organization repository ID.".into()),
        draft,
    )
    .await
}
