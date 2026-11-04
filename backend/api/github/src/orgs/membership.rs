use crate::auth;
use crate::orgs::membership_document;
use crate::orgs::outcome;
use crate::orgs::target;
use axum::extract::Path;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::routing::delete;
use axum::routing::put;
use axum::routing::MethodRouter;
use axum::Json;
use codeyard_app::app::AppState;
use codeyard_db_core::models::OrgRole;
use codeyard_db_core::queries::organizations::OrganizationOutcome;
use codeyard_db_orgs::organizations::discovery;
use codeyard_model::github_error::Error;
use codeyard_model::github_error::Result;
use serde_json::json;
use serde_json::Value;
use std::sync::Arc;

pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        ("orgs/set-membership-for-user", put(set)),
        ("orgs/remove-membership-for-user", delete(remove_membership)),
        ("orgs/remove-member", delete(remove_member)),
    ]
}

async fn set(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((name, username)): Path<(String, String)>,
    body: axum::body::Bytes,
) -> Result<Json<Value>> {
    let actor = auth::authenticate(&state, &headers, true)
        .await?
        .ok_or_else(auth::required)?;
    actor.require_write()?;
    let input: Value = if body.is_empty() {
        json!({})
    } else {
        serde_json::from_slice(&body).map_err(codeyard_model::github_error::json_parse_error)?
    };
    let object = input
        .as_object()
        .ok_or_else(|| Error::validation("Membership", "body", "invalid"))?;
    let role = match object.get("role") {
        None => "member",
        Some(Value::String(role)) => role,
        _ => return Err(Error::validation("Membership", "role", "invalid")),
    };
    let role = match role {
        "admin" => OrgRole::Owner,
        "member" => OrgRole::Member,
        _ => return Err(Error::validation("Membership", "role", "invalid")),
    };
    let org = target(&state, &name).await?;
    let user = codeyard_db_identity::users::accounts::get_user_by_username(&state.pool, &username)
        .await
        .map_err(Error::unavailable)?
        .ok_or_else(|| Error::validation("Membership", "user", "invalid"))?;
    let active = discovery::list_membership_states(
        &state.pool,
        actor.user.id,
        Some(org.id),
        user.id,
        Some("active"),
        (0, 1),
    )
    .await
    .map_err(Error::unavailable)?;
    let membership = if active.is_empty() {
        let invitation = outcome(
            super::set_organization_invitation_by_owner(
                &state.pool,
                actor.database_actor(),
                org.id,
                user.id,
                role,
            )
            .await
            .map_err(Error::unavailable)?,
        )?;
        codeyard_notifications::create::organization_invitation(
            &state.pool,
            &state.notification_broadcaster,
            actor.user.id,
            &invitation,
        )
        .await;
        membership_document(&org, &user, &invitation.role, "pending")
    } else {
        let member = outcome(
            super::update_organization_member_role_by_owner(
                &state.pool,
                actor.database_actor(),
                org.id,
                user.id,
                role,
            )
            .await
            .map_err(Error::unavailable)?,
        )?;
        membership_document(&org, &user, &member.role, "active")
    };
    Ok(Json(membership))
}

async fn remove(
    state: &AppState,
    headers: &HeaderMap,
    name: &str,
    username: &str,
    missing_ok: bool,
) -> Result<StatusCode> {
    let actor = auth::authenticate(state, headers, true)
        .await?
        .ok_or_else(auth::required)?;
    actor.require_write()?;
    let org = target(state, name).await?;
    let user = codeyard_db_identity::users::accounts::get_user_by_username(&state.pool, username)
        .await
        .map_err(Error::unavailable)?
        .map_or(-1, |user| user.id);
    let result = super::remove_member_or_invitation_by_owner(
        &state.pool,
        actor.database_actor(),
        org.id,
        user,
    )
    .await
    .map_err(Error::unavailable)?;
    if matches!(result, OrganizationOutcome::Conflict) {
        return Err(Error::new(
            StatusCode::FORBIDDEN,
            "Cannot remove the last organization owner",
        ));
    }
    if !(missing_ok && matches!(result, OrganizationOutcome::Missing)) {
        outcome(result)?;
    }
    Ok(StatusCode::NO_CONTENT)
}
async fn remove_membership(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((name, username)): Path<(String, String)>,
) -> Result<StatusCode> {
    remove(&state, &headers, &name, &username, false).await
}
async fn remove_member(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((name, username)): Path<(String, String)>,
) -> Result<StatusCode> {
    remove(&state, &headers, &name, &username, true).await
}

use crate::orgs::membership;
