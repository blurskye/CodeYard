use axum::extract::Path;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::http::Uri;
use axum::response::Response;
use axum::routing::get;
use axum::Form;
use axum::Router;
use codeyard_api_http::context::PageContext;
use codeyard_api_http::render::redirect_response;
use codeyard_app::app::AppState;
use codeyard_webui::app::Page;
use codeyard_webui::pages::orgs::code_security as view;
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;

pub(super) fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/settings/organizations/:org/security", get(show))
        .route(
            "/settings/organizations/:org/security/:action",
            axum::routing::post(post),
        )
        .layer(axum::extract::DefaultBodyLimit::max(16 * 1024))
}
fn failure(ctx: &PageContext, error: anyhow::Error) -> Response {
    let (status, message) =
        match error.downcast_ref::<codeyard_db_core::queries::code_security::Refusal>() {
            Some(codeyard_db_core::queries::code_security::Refusal::Missing) => {
                (StatusCode::NOT_FOUND, "Security configuration not found.")
            }
            Some(codeyard_db_core::queries::code_security::Refusal::Denied) => (
                StatusCode::FORBIDDEN,
                "Organization security access is required.",
            ),
            Some(codeyard_db_core::queries::code_security::Refusal::Duplicate) => (
                StatusCode::UNPROCESSABLE_ENTITY,
                "A configuration with this name already exists.",
            ),
            Some(codeyard_db_core::queries::code_security::Refusal::Invalid) => (
                StatusCode::UNPROCESSABLE_ENTITY,
                "Check the configuration name, scope and repository IDs.",
            ),
            None => (
                StatusCode::SERVICE_UNAVAILABLE,
                "Security configurations are temporarily unavailable.",
            ),
        };
    ctx.error(status, message)
}
async fn show(
    State(state): State<Arc<AppState>>,
    Path(org): Path<i64>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    let actor = match ctx.require_user() {
        Ok(user) => user.id,
        Err(response) => return response,
    };
    let before = match ctx.query.get("before") {
        None => None,
        Some(value) => match value.parse::<i64>() {
            Ok(id) if id > 0 => Some(id),
            _ => return ctx.error(StatusCode::BAD_REQUEST, "Invalid configuration page."),
        },
    };
    let Some(session) = ctx.session_id.as_deref() else {
        return ctx.error(StatusCode::FORBIDDEN, "A live account session is required.");
    };
    let loaded = match codeyard_db_security::code_security::settings_page(
        &ctx.state.pool,
        actor,
        session,
        org,
        before,
    )
    .await
    {
        Ok(page) => page,
        Err(error) => return failure(&ctx, error),
    };
    let mut configurations = Vec::new();
    for row in loaded.configurations {
        let value: serde_json::Value = match serde_json::from_str(&row.settings) {
            Ok(value) => value,
            Err(error) => return failure(&ctx, error.into()),
        };
        configurations.push(view::Configuration {
            id: row.id,
            name: row.name,
            description: value["description"].as_str().unwrap_or("").to_owned(),
            settings: view::TOGGLES
                .iter()
                .map(|(name, _)| {
                    (
                        (*name).to_owned(),
                        value[*name].as_str().unwrap_or("not_set").to_owned(),
                    )
                })
                .collect(),
            default_for_new_repos: row.default_for_new_repos,
        });
    }
    ctx.page_model(
        "Code security configurations",
        Page::CodeSecurity(view::Page {
            org_id: org,
            org_name: loaded.organization_name,
            csrf_token: ctx.csrf_token.clone(),
            configurations,
            writable: loaded.writable,
            before,
            next_before: loaded.next_before,
        }),
    )
}
async fn post(
    State(state): State<Arc<AppState>>,
    Path((org, action)): Path<(i64, String)>,
    headers: HeaderMap,
    uri: Uri,
    Form(values): Form<HashMap<String, String>>,
) -> Response {
    let ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    if let Err(response) = ctx.check_csrf(&values) {
        return response;
    }
    let actor = match ctx.require_user() {
        Ok(user) => user.id,
        Err(response) => return response,
    };
    let actor = ctx.database_actor(actor);
    let id = values
        .get("configuration_id")
        .map(|id| id.parse::<i64>())
        .transpose();
    let id = match id {
        Ok(id) => id,
        Err(_) => {
            return ctx.error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "Invalid configuration ID.",
            )
        }
    };
    let result = match (action.as_str(), id) {
        ("save", id) => {
            let mut patch = json!({"name":values.get("name").map_or("",String::as_str),"description":values.get("description").map_or("",String::as_str)});
            for (name, _) in view::TOGGLES {
                patch[name] = json!(values.get(name).map_or("not_set", String::as_str));
            }
            codeyard_db_security::code_security::save(&ctx.state.pool, actor, org, id, &patch)
                .await
                .map(|_| ())
        }
        ("delete", Some(id)) => {
            codeyard_db_security::code_security::delete(&ctx.state.pool, actor, org, id).await
        }
        ("defaults", Some(id)) => codeyard_db_security::code_security::set_default(
            &ctx.state.pool,
            actor,
            org,
            id,
            values
                .get("default_for_new_repos")
                .map_or("", String::as_str),
        )
        .await
        .map(|_| ()),
        ("attach", Some(id)) => {
            let selected = values
                .get("repository_ids")
                .map_or("", String::as_str)
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::parse::<i64>)
                .collect::<Result<Vec<_>, _>>();
            let selected = match selected {
                Ok(ids) => ids,
                Err(_) => {
                    return ctx.error(
                        StatusCode::UNPROCESSABLE_ENTITY,
                        "Enter comma-separated repository IDs.",
                    )
                }
            };
            codeyard_db_security::code_security::attach(
                &ctx.state.pool,
                actor,
                org,
                Some(id),
                values.get("scope").map_or("", String::as_str),
                &selected,
            )
            .await
        }
        _ => return ctx.error(StatusCode::NOT_FOUND, "Action not found."),
    };
    match result {
        Ok(()) => redirect_response(&format!("/settings/organizations/{org}/security")),
        Err(error) => failure(&ctx, error),
    }
}
