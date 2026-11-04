use super::{auth, discovery, membership_document, outcome, target, AppState, Error, Result};
use crate::backend::db;
use crate::backend::db::organizations;
use crate::backend::db::organizations::OrganizationOutcome;
use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    routing::{delete, put, MethodRouter},
    Json,
};
use codeyard_db_core::models::OrgRole;
use serde_json::{json, Value};
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
        serde_json::from_slice(&body).map_err(super::super::json_parse_error)?
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
        "admin" => codeyard_db_core::models::OrgRole::Owner,
        "member" => codeyard_db_core::models::OrgRole::Member,
        _ => return Err(Error::validation("Membership", "role", "invalid")),
    };
    let org = target(&state, &name).await?;
    let user = db::users::accounts::get_user_by_username(&state.pool, &username)
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
            organizations::set_organization_invitation_by_owner(
                &state.pool,
                actor.database_actor(),
                org.id,
                user.id,
                role,
            )
            .await
            .map_err(Error::unavailable)?,
        )?;
        crate::backend::notifications::create::organization_invitation(
            &state.pool,
            &state.notification_broadcaster,
            actor.user.id,
            &invitation,
        )
        .await;
        membership_document(&org, &user, &invitation.role, "pending")
    } else {
        let member = outcome(
            organizations::update_organization_member_role_by_owner(
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
    let user = db::users::accounts::get_user_by_username(&state.pool, username)
        .await
        .map_err(Error::unavailable)?
        .map_or(-1, |user| user.id);
    let result = organizations::remove_member_or_invitation_by_owner(
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
