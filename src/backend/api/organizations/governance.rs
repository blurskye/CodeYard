//! Exceptional audited ingress: explicit IDs, current verified staff, no tenant reads.

use super::organization::Fields;
use super::{membership, organization, INDEX};
use crate::backend::api::context::PageContext;
use crate::backend::app::AppState;
use crate::backend::db::organizations::{
    list_ownerless_organizations, recover_suspended_organization_owner, OrganizationOutcome,
};
use axum::extract::rejection::FormRejection;
use axum::extract::{Form, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::Response;
use std::sync::Arc;

pub(super) const RECOVERY: &str = "/admin/organizations/recovery";

pub(super) async fn recover(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
    form: Result<Form<Fields>, FormRejection>,
) -> Response {
    let mut ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    let actor = match ctx.require_user() {
        Ok(user) if user.is_admin && !user.is_banned && !user.must_change_password => user.id,
        Ok(_) => {
            return ctx.error(
                StatusCode::FORBIDDEN,
                "Organization recovery is not permitted.",
            )
        }
        Err(response) => return response,
    };
    let form =
        match form {
            Ok(Form(form)) => form,
            Err(rejection) => return ctx.error(
                rejection.status(),
                "Unable to read the organization recovery form. Submit a bounded URL-encoded form.",
            ),
        };
    if let Err(response) = ctx.check_csrf(&form) {
        return response;
    }
    // Recovery neither follows caller redirects nor reads the supplied tenant.
    // On errors render the operator's own index with only bounded safe draft IDs.
    ctx.query.clear();
    let draft = if organization::field(&form, "action") == "recover-owner" {
        organization::draft(&form)
    } else {
        Fields::new()
    };
    let ids = if organization::bounded(&form)
        && organization::field(&form, "action") == "recover-owner"
    {
        organization::id(&form, "org_id")
            .ok()
            .zip(organization::username(&form).ok())
    } else {
        None
    };
    let Some((org, username)) = ids else {
        return organization::respond(
            &ctx,
            None,
            StatusCode::BAD_REQUEST,
            Some("Choose an organization and enter the new owner's username.".into()),
            draft,
        )
        .await;
    };
    // The typed name must match the organization being recovered, and only ownerless ones are offered.
    let ownerless = match list_ownerless_organizations(&ctx.state.pool, actor).await {
        Ok(OrganizationOutcome::Applied(values)) => values,
        Ok(_) => vec![],
        Err(_) => {
            return ctx.error(
                StatusCode::SERVICE_UNAVAILABLE,
                "Organization recovery is temporarily unavailable; no success was confirmed.",
            )
        }
    };
    let Some(recovered) = ownerless.iter().find(|candidate| candidate.id == org) else {
        return organization::respond(
            &ctx,
            None,
            StatusCode::CONFLICT,
            Some("Recovery was not applied. Confirm suspended governance and an eligible current member.".into()),
            draft,
        )
        .await;
    };
    if organization::field(&form, "confirm") != recovered.name {
        return organization::respond(
            &ctx,
            None,
            StatusCode::BAD_REQUEST,
            Some("Type the organization name to confirm recovery.".into()),
            draft,
        )
        .await;
    }
    let target = match membership::target(&ctx, &username).await {
        Ok(Some(target)) => target,
        Ok(None) => {
            return organization::respond(
                &ctx,
                None,
                StatusCode::NOT_FOUND,
                Some(format!("No user named {username}.")),
                draft,
            )
            .await
        }
        Err(response) => return response,
    };
    // DAO atomically rechecks persisted staff/active status, membership, suspended
    // governance and mandatory audit. No ordinary tenant operation uses this gate.
    let (status, message) = match recover_suspended_organization_owner(&ctx.state.pool, actor, org, target).await {
        Ok(OrganizationOutcome::Applied(_)) => return ctx.redirect(INDEX),
        Ok(_) => (StatusCode::CONFLICT, "Recovery was not applied. Confirm suspended governance and an eligible current member."),
        Err(_) => (StatusCode::SERVICE_UNAVAILABLE, "Organization recovery is temporarily unavailable; no success was confirmed."),
    };
    organization::respond(&ctx, None, status, Some(message.into()), draft).await
}
