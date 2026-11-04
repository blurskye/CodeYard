//! Invitations remain offers until a verified recipient accepts through REST or native settings.
use super::{auth, outcome, target, AppState, Error, Result};
use crate::backend::api::github::pagination::Pagination;
use crate::backend::db;
use crate::backend::db::organizations;
use crate::backend::db::organizations::InvitationFilter;
use crate::backend::db::organizations::InvitationTeams;
use crate::backend::db::organizations::NewInvitation;
use crate::backend::webhooks::payloads;
use axum::{
    extract::{OriginalUri, Path, Query, State},
    http::{HeaderMap, StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::{delete, get, post, MethodRouter},
    Json,
};
use codeyard_db_core::models::OrgInvitation;
use codeyard_db_core::models::OrgRole;
use codeyard_db_core::models::Organization;
use serde::Deserialize;
use serde_json::Value;
use std::{collections::HashMap, sync::Arc};

pub(super) fn handlers() -> Vec<(&'static str, MethodRouter<Arc<AppState>>)> {
    vec![
        ("orgs/create-invitation", post(create)),
        ("orgs/list-pending-invitations", get(pending)),
        ("orgs/list-failed-invitations", get(failed)),
        ("orgs/cancel-invitation", delete(cancel)),
        ("orgs/list-invitation-teams", get(teams)),
    ]
}
async fn context(
    state: &AppState,
    headers: &HeaderMap,
    name: &str,
    write: bool,
) -> Result<(Organization, auth::Authentication)> {
    let actor = auth::authenticate(state, headers, true)
        .await?
        .ok_or_else(auth::required)?;
    if write {
        actor.require_write()?;
    }
    Ok((target(state, name).await?, actor))
}
async fn documents(
    state: &AppState,
    org: &Organization,
    invitations: Vec<OrgInvitation>,
) -> Result<Vec<Value>> {
    let ids = invitations
        .iter()
        .flat_map(|invite| [invite.user_id, invite.invited_by_id])
        .flatten()
        .collect::<Vec<_>>();
    let users = db::users::accounts::get_users_by_ids(&state.pool, &ids)
        .await
        .map_err(Error::unavailable)?
        .into_iter()
        .map(|user| (user.id, user))
        .collect::<HashMap<_, _>>();
    let ids = invitations
        .iter()
        .map(|invite| invite.id)
        .collect::<Vec<_>>();
    let counts = organizations::invitation_team_counts(&state.pool, &ids)
        .await
        .map_err(Error::unavailable)?;
    Ok(invitations
        .iter()
        .map(|invite| {
            payloads::organization_invitation(
                invite,
                org,
                invite.user_id.and_then(|id| users.get(&id)),
                invite.invited_by_id.and_then(|id| users.get(&id)),
                counts.get(&invite.id).copied().unwrap_or(0),
            )
        })
        .collect())
}
#[derive(Deserialize)]
struct Input {
    invitee_id: Option<i64>,
    email: Option<String>,
    role: Option<String>,
    team_ids: Option<Vec<i64>>,
}
async fn create(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(name): Path<String>,
    body: std::result::Result<Json<Input>, axum::extract::rejection::JsonRejection>,
) -> Result<Response> {
    let (org, actor) = context(&state, &headers, &name, true).await?;
    let Json(input) = body.map_err(super::super::json_error)?;
    let role = match input.role.as_deref().unwrap_or("direct_member") {
        "admin" => OrgRole::Owner,
        "direct_member" | "reinstate" => OrgRole::Member,
        "billing_manager" => return Err(Error::invalid("Billing manager invitations require a billing service, which Codeyard does not provide")),
        _ => return Err(Error::validation("OrganizationInvitation", "role", "invalid")),
    };
    let input = NewInvitation {
        user_id: input.invitee_id,
        email: input.email.map(|email| email.trim().to_ascii_lowercase()),
        role,
        reinstate: input.role.as_deref() == Some("reinstate"),
        teams: input.team_ids.map(InvitationTeams::Ids),
    };
    if !input.valid() {
        return Err(Error::validation(
            "OrganizationInvitation",
            "body",
            "invalid",
        ));
    }
    let invitation = outcome(
        organizations::create_organization_invitation(
            &state.pool,
            actor.database_actor(),
            org.id,
            &input,
            true,
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
    let body = documents(&state, &org, vec![invitation])
        .await?
        .pop()
        .unwrap();
    Ok((StatusCode::CREATED, Json(body)).into_response())
}
#[derive(Deserialize)]
struct PendingQuery {
    page: Option<usize>,
    per_page: Option<usize>,
    role: Option<String>,
    invitation_source: Option<String>,
}
async fn listing(
    state: &AppState,
    headers: &HeaderMap,
    name: &str,
    filter: InvitationFilter,
    page: Pagination,
    uri: &Uri,
) -> Result<Response> {
    let (org, actor) = context(state, headers, name, false).await?;
    let (offset, limit) = page.window()?;
    let invitations = outcome(
        organizations::invitation_page(
            &state.pool,
            actor.user.id,
            org.id,
            &filter,
            (offset as i64, (limit + 1) as i64),
        )
        .await
        .map_err(Error::unavailable)?,
    )?;
    let next = invitations.len() > limit;
    page.response(uri, documents(state, &org, invitations).await?, next)
}
async fn pending(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Query(query): Query<PendingQuery>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let mut filter = InvitationFilter {
        owner_only: true,
        ..Default::default()
    };
    match query.role.as_deref().unwrap_or("all") {
        "all" => {}
        "admin" => filter.role = Some(OrgRole::Owner),
        "direct_member" => filter.role = Some(OrgRole::Member),
        "billing_manager" | "hiring_manager" => filter.empty = true,
        _ => {
            return Err(Error::validation(
                "OrganizationInvitation",
                "role",
                "invalid",
            ))
        }
    }
    match query.invitation_source.as_deref().unwrap_or("all") {
        "all" | "member" => {}
        "scim" => filter.empty = true,
        _ => {
            return Err(Error::validation(
                "OrganizationInvitation",
                "invitation_source",
                "invalid",
            ))
        }
    }
    listing(
        &state,
        &headers,
        &name,
        filter,
        Pagination {
            page: query.page,
            per_page: query.per_page,
        },
        &uri,
    )
    .await
}
async fn failed(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Query(page): Query<Pagination>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    listing(
        &state,
        &headers,
        &name,
        InvitationFilter {
            failed: true,
            owner_only: true,
            ..Default::default()
        },
        page,
        &uri,
    )
    .await
}
fn invitation_id(params: &HashMap<String, String>) -> Result<i64> {
    params
        .get("invitation_id")
        .and_then(|id| id.parse().ok())
        .filter(|id| *id > 0)
        .ok_or_else(Error::missing)
}
async fn cancel(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<HashMap<String, String>>,
) -> Result<StatusCode> {
    let (org, actor) = context(&state, &headers, &params["org"], true).await?;
    outcome(
        organizations::cancel_invitation_id(
            &state.pool,
            actor.database_actor(),
            org.id,
            invitation_id(&params)?,
            true,
        )
        .await
        .map_err(Error::unavailable)?,
    )?;
    Ok(StatusCode::NO_CONTENT)
}
async fn teams(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(params): Path<HashMap<String, String>>,
    Query(page): Query<Pagination>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response> {
    let (org, actor) = context(&state, &headers, &params["org"], false).await?;
    let (offset, limit) = page.window()?;
    let teams = outcome(
        organizations::invitation_teams(
            &state.pool,
            actor.user.id,
            org.id,
            invitation_id(&params)?,
            (offset as i64, (limit + 1) as i64),
        )
        .await
        .map_err(Error::unavailable)?,
    )?;
    let next = teams.len() > limit;
    let rows = teams
        .into_iter()
        .map(|team| {
            payloads::team(&db::repositories::push_teams::TeamAccess {
                team,
                org_slug: org.name.clone(),
                permission: 1,
            })
        })
        .collect();
    page.response(&uri, rows, next)
}
