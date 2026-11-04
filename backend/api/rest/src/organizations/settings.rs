//! Session-bound, bounded owner controls for the same organization policies used by REST.
use axum::response::IntoResponse;

use crate::organizations::organization::Fields;
use axum::extract::Form;
use axum::extract::Path;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::http::Uri;
use axum::response::Response;
use axum::routing::post;
use axum::Router;
use codeyard_api_http::context::PageContext;
use codeyard_app::app::AppState;
use codeyard_webui::pages::orgs as view;
use serde_json::Map;
use serde_json::Value;
use std::collections::HashSet;
use std::sync::Arc;
use OrganizationOutcome::Applied;
use OrganizationOutcome::Conflict;
use OrganizationOutcome::Denied;
use OrganizationOutcome::Missing;
use OrganizationOutcome::Suspended;

pub(super) fn router() -> Router<Arc<AppState>> {
    Router::new().route("/settings/organizations/:id/settings", post(mutate))
}
pub(super) async fn panel(
    ctx: &PageContext,
    actor: i64,
    org: i64,
) -> Result<view::SettingsPanel, Response> {
    let updated =
        match codeyard_db_orgs::organizations::settings::owned(&ctx.state.pool, actor, org).await {
            Ok(Applied(updated)) => updated,
            Ok(_) => return Err(crate::organizations::organization::not_found(ctx)),
            Err(_) => return Err(crate::organizations::organization::unavailable(ctx)),
        };
    let settings = updated.settings;
    Ok(view::SettingsPanel {
        company: settings.company.unwrap_or_default(),
        email: settings.email.unwrap_or_default(),
        twitter_username: settings.twitter_username.unwrap_or_default(),
        location: settings.location.unwrap_or_default(),
        blog: settings.blog.unwrap_or_default(),
        billing_email: updated.organization.billing_email.unwrap_or_default(),
        default_repository_permission: match settings.default_repository_permission {
            1 => "read",
            3 => "write",
            5 => "admin",
            _ => "none",
        }
        .into(),
        has_organization_projects: settings.has_organization_projects,
        members_can_create_public_repositories: settings.members_can_create_public_repositories,
        members_can_create_private_repositories: settings.members_can_create_private_repositories,
        members_can_fork_private_repositories: settings.members_can_fork_private_repositories,
        web_commit_signoff_required: settings.web_commit_signoff_required,
        deploy_keys_enabled_for_repositories: settings.deploy_keys_enabled_for_repositories,
    })
}
fn allowed(action: &str) -> &'static [&'static str] {
    match action {
        "save-organization-contacts" => &[
            "company",
            "email",
            "twitter_username",
            "location",
            "blog",
            "billing_email",
        ],
        "save-organization-repository-policy" => &[
            "default_repository_permission",
            "members_can_create_public_repositories",
            "members_can_create_private_repositories",
            "members_can_fork_private_repositories",
            "has_organization_projects",
        ],
        "save-organization-security-policy" => &[
            "web_commit_signoff_required",
            "deploy_keys_enabled_for_repositories",
        ],
        _ => &[],
    }
}
fn bound(key: &str) -> usize {
    match key {
        "company" | "location" | "blog" => 1024,
        "email" | "billing_email" => 254,
        "twitter_username" => 15,
        "action" | "csrf_token" => 64,
        _ => 32,
    }
}
fn valid(form: &Fields) -> bool {
    let keys = allowed(codeyard_api_http::shared::user::responses::field(
        form, "action",
    ));
    !keys.is_empty()
        && form.len() <= keys.len() + 2
        && form.iter().all(|(key, value)| {
            (matches!(key.as_str(), "action" | "csrf_token") || keys.contains(&key.as_str()))
                && value.len() <= bound(key)
        })
}
fn changes(
    form: &Fields,
) -> Result<
    codeyard_db_orgs::organizations::settings::Changes,
    codeyard_db_orgs::organizations::settings::Invalid,
> {
    let mut input = Map::new();
    for &key in allowed(codeyard_api_http::shared::user::responses::field(
        form, "action",
    )) {
        if key == "has_organization_projects"
            || key.starts_with("members_can_")
            || matches!(
                key,
                "web_commit_signoff_required" | "deploy_keys_enabled_for_repositories"
            )
        {
            let enabled = match form.get(key).map(String::as_str) {
                None | Some("false") => false,
                Some("true") => true,
                _ => {
                    return Err(codeyard_db_orgs::organizations::settings::Invalid {
                        field: key,
                        message: "Invalid checkbox value",
                    })
                }
            };
            input.insert(key.into(), Value::Bool(enabled));
        } else if let Some(value) = form.get(key) {
            input.insert(key.into(), Value::String(value.clone()));
        }
    }
    codeyard_db_orgs::organizations::settings::Changes::parse(&Value::Object(input))
}
async fn mutate(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
    Path(org): Path<i64>,
    input: Result<Form<Vec<(String, String)>>, axum::extract::rejection::FormRejection>,
) -> Response {
    let mut ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    ctx.query.insert("tab".into(), "settings".into());
    let Form(pairs) = match input {
        Ok(input) => input,
        Err(error) => return error.into_response(),
    };
    let mut seen = HashSet::new();
    if pairs.len() > 16 || pairs.iter().any(|(key, _)| !seen.insert(key)) {
        return crate::organizations::organization::invalid(&ctx);
    }
    let form: Fields = pairs.into_iter().collect();
    if let Err(response) = ctx.check_csrf(&form) {
        return response;
    }
    let actor = match ctx.require_user() {
        Ok(user) => user.id,
        Err(response) => return response,
    };
    if !valid(&form) {
        return crate::organizations::organization::invalid(&ctx);
    }
    if let Err(response) = crate::organizations::organization::selected(&ctx, actor, org).await {
        return response;
    }
    let draft = form
        .iter()
        .filter(|(key, _)| key.as_str() != "csrf_token")
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    let result = match changes(&form) {
        Ok(input) => {
            codeyard_db_orgs::organizations::settings::change(
                &ctx.state.pool,
                ctx.database_actor(actor),
                org,
                &input,
            )
            .await
        }
        Err(error) => {
            return crate::organizations::organization::respond(
                &ctx,
                Some(org),
                StatusCode::BAD_REQUEST,
                Some(format!("{}: {}", error.field, error.message)),
                draft,
            )
            .await
        }
    };
    let (status, message) = match result {
        Ok(Applied(_)) => {
            return ctx.redirect(&format!(
                "{}/{org}?tab=settings",
                crate::organizations::INDEX
            ))
        }
        Ok(Denied) => (
            StatusCode::FORBIDDEN,
            "Only organization owners can change organization settings.",
        ),
        Ok(Missing) => (StatusCode::NOT_FOUND, "Organization not found."),
        Ok(Suspended) => (
            StatusCode::CONFLICT,
            "Organization governance is suspended.",
        ),
        Ok(Conflict) => (
            StatusCode::CONFLICT,
            "Organization settings changed. Reload and try again.",
        ),
        Err(_) => return crate::organizations::organization::unavailable(&ctx),
    };
    crate::organizations::organization::respond(
        &ctx,
        Some(org),
        status,
        Some(message.into()),
        draft,
    )
    .await
}
