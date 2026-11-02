use super::{auth, outcome, target, AppState, Error, Result};
use crate::backend::{
    api::http::config::link_origin,
    db::{
        models::Organization,
        organizations::{
            discovery,
            settings::{self, Changes, Settings},
        },
    },
    webhooks::payloads,
};
use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    routing::{patch, MethodRouter},
    Json,
};
use serde_json::{json, Value};
use std::sync::Arc;

pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![("orgs/update", patch(update))]
}
async fn update(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Json(input): Json<Value>,
) -> Result<Json<Value>> {
    let actor = auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(auth::required)?;
    actor.require_write()?;
    let org = target(&state, &name).await?;
    let input=Changes::parse(&input).map_err(|error|Error(StatusCode::UNPROCESSABLE_ENTITY,"Validation Failed".into(),Some(json!([{"resource":"Organization","field":error.field,"code":"invalid","message":error.message}]))))?;
    let updated = outcome(
        settings::change(&state.pool, actor.user.id, org.id, &input)
            .await
            .map_err(Error::unavailable)?,
    )?;
    document(&state, &updated.organization, &updated.settings, true)
        .await
        .map(Json)
}
pub(super) async fn document(
    state: &AppState,
    org: &Organization,
    settings: &Settings,
    owner: bool,
) -> Result<Value> {
    let mut value = payloads::organization(org);
    value["name"] = json!(org.display_name.as_deref().unwrap_or(&org.name));
    value["html_url"] = json!(format!(
        "{}{}",
        link_origin(),
        crate::webui::profile_path(&org.name)
    ));
    value["type"] = json!("Organization");
    value["is_verified"] = json!(org.is_verified);
    value["public_repos"] = json!(
        discovery::organization_public_repositories(&state.pool, org.id)
            .await
            .map_err(Error::unavailable)?
    );
    for field in ["public_gists", "followers", "following"] {
        value[field] = json!(0);
    }
    value["has_organization_projects"] = json!(settings.has_organization_projects);
    value["has_repository_projects"] = json!(false);
    value["created_at"] = json!(org.created_at);
    value["updated_at"] = json!(org.updated_at);
    value["archived_at"] = Value::Null;
    value["company"] = json!(settings.company);
    value["email"] = json!(settings.email);
    value["twitter_username"] = json!(settings.twitter_username);
    value["location"] = json!(settings.location);
    value["blog"] = json!(settings.blog);
    if owner {
        value["billing_email"] = json!(org.billing_email);
        value["default_repository_permission"] =
            json!(match settings.default_repository_permission {
                1 => "read",
                3 => "write",
                5 => "admin",
                _ => "none",
            });
        let public = settings.members_can_create_public_repositories;
        let private = settings.members_can_create_private_repositories;
        value["members_can_create_repositories"] = json!(public || private);
        value["members_can_create_public_repositories"] = json!(public);
        value["members_can_create_private_repositories"] = json!(private);
        value["members_allowed_repository_creation_type"] = json!(if public {
            "all"
        } else if private {
            "private"
        } else {
            "none"
        });
        for field in [
            "members_can_create_internal_repositories",
            "members_can_create_pages",
            "members_can_create_public_pages",
            "members_can_create_private_pages",
        ] {
            value[field] = json!(false);
        }
        value["members_can_fork_private_repositories"] =
            json!(settings.members_can_fork_private_repositories);
        value["web_commit_signoff_required"] = json!(settings.web_commit_signoff_required);
        value["deploy_keys_enabled_for_repositories"] =
            json!(settings.deploy_keys_enabled_for_repositories);
    }
    Ok(value)
}
