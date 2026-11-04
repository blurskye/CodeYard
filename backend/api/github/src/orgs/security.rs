//! Deprecated security-manager team operations alias the native predefined organization role.
use crate::orgs::outcome;
use crate::orgs::target;
use axum::body::Bytes;
use axum::extract::Path;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::routing::delete;
use axum::routing::get;
use axum::routing::post;
use axum::routing::put;
use axum::routing::MethodRouter;
use axum::Json;
use codeyard_app::app::AppState;
use codeyard_db_core::queries::organizations::roles::SECURITY_MANAGER_ROLE_ID;
use codeyard_db_orgs::organizations::roles::RoleTarget;
use codeyard_db_repos::repositories::push_teams::TeamAccess;
use codeyard_model::github_error::Error;
use codeyard_model::github_error::Result;
use serde_json::Value;
use std::sync::Arc;

pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        ("orgs/list-security-manager-teams", get(teams)),
        ("orgs/add-security-manager-team", put(assign)),
        ("orgs/remove-security-manager-team", delete(revoke)),
        (
            "orgs/enable-or-disable-security-product-on-all-org-repos",
            post(product),
        ),
    ]
}
async fn product(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((name, product, enablement)): Path<(String, String, String)>,
    body: Bytes,
) -> Result<StatusCode> {
    let actor = codeyard_identity::github_auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(codeyard_identity::github_auth::required)?;
    actor.require_write()?;
    let org = target(&state, &name).await?;
    let role = outcome(
        codeyard_db_orgs::organizations::membership::get_organization_role(
            &state.pool,
            actor.user.id,
            org.id,
        )
        .await
        .map_err(Error::unavailable)?,
    )?;
    if role != codeyard_db_core::models::OrgRole::Owner {
        return Err(Error::new(
            StatusCode::FORBIDDEN,
            "Must be an organization owner",
        ));
    }
    if !matches!(
        product.as_str(),
        "dependency_graph"
            | "dependabot_alerts"
            | "dependabot_security_updates"
            | "advanced_security"
            | "code_scanning_default_setup"
            | "secret_scanning"
            | "secret_scanning_push_protection"
    ) {
        return Err(Error::validation(
            "Organization",
            "security_product",
            "invalid",
        ));
    }
    if !matches!(enablement.as_str(), "enable_all" | "disable_all") {
        return Err(Error::validation("Organization", "enablement", "invalid"));
    }
    let input: Value = if body.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&body).map_err(codeyard_model::github_error::json_parse_error)?
    };
    if !input.is_null() {
        let fields = input
            .as_object()
            .ok_or_else(|| Error::invalid("Invalid security product request"))?;
        if fields.keys().any(|key| key != "query_suite")
            || fields
                .get("query_suite")
                .is_some_and(|value| !matches!(value.as_str(), Some("default" | "extended")))
        {
            return Err(Error::validation("Organization", "query_suite", "invalid"));
        }
    }
    // None of these products has a native configuration or worker in this branch.
    Err(Error::new(
        StatusCode::UNPROCESSABLE_ENTITY,
        "This security product is not available in Codeyard",
    ))
}
async fn teams(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Result<Json<Vec<Value>>> {
    let actor = codeyard_identity::github_auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(codeyard_identity::github_auth::required)?;
    let org = target(&state, &name).await?;
    // This legacy operation has no pagination. Refuse an oversized response, never silently truncate it.
    let teams = outcome(
        codeyard_db_orgs::organizations::roles::teams(
            &state.pool,
            actor.user.id,
            org.id,
            SECURITY_MANAGER_ROLE_ID,
            0,
            (0, 10_001),
        )
        .await
        .map_err(Error::unavailable)?,
    )?;
    if teams.len() > 10_000 {
        return Err(Error::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "Use paginated organization role team assignments for this organization",
        ));
    }
    Ok(Json(
        teams
            .into_iter()
            .map(|team| {
                codeyard_webhooks::payloads::team(&TeamAccess {
                    team,
                    org_slug: org.name.clone(),
                    permission: 1,
                })
            })
            .collect(),
    ))
}
async fn change(
    state: &AppState,
    headers: &HeaderMap,
    name: &str,
    team: String,
    assigned: bool,
) -> Result<StatusCode> {
    let actor = codeyard_identity::github_auth::authenticate(state, headers, true)
        .await?
        .ok_or_else(codeyard_identity::github_auth::required)?;
    actor.require_write()?;
    let org = target(state, name).await?;
    outcome(
        codeyard_db_orgs::organizations::roles::change(
            &state.pool,
            actor.database_actor(),
            org.id,
            &RoleTarget::Team(team),
            Some(SECURITY_MANAGER_ROLE_ID),
            assigned,
        )
        .await
        .map_err(Error::unavailable)?,
    )?;
    Ok(StatusCode::NO_CONTENT)
}
async fn assign(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((name, team)): Path<(String, String)>,
) -> Result<StatusCode> {
    change(&state, &headers, &name, team, true).await
}
async fn revoke(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((name, team)): Path<(String, String)>,
) -> Result<StatusCode> {
    change(&state, &headers, &name, team, false).await
}
