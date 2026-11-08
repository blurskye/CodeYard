//! Exceptional audited ingress: explicit IDs, current verified staff, no tenant reads.

use crate::organizations::organization::Fields;
use crate::organizations::INDEX;
use axum::extract::rejection::FormRejection;
use axum::extract::Form;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::http::Uri;
use axum::response::Response;
use codeyard_api_http::context::PageContext;
use codeyard_app::app::AppState;
use codeyard_database::queries::organizations::OrganizationOutcome;
use codeyard_db_orgs::organizations::recovery::list_ownerless_organizations;
use codeyard_db_orgs::organizations::recovery::recover_suspended_organization_owner;
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
    let Some(actor) = ctx.administrator().map(|user| user.id) else {
        return match ctx.require_user() {
            Ok(_) => ctx.error(
                StatusCode::FORBIDDEN,
                "Organization recovery is not permitted.",
            ),
            Err(response) => response,
        };
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
    let draft =
        if codeyard_api_http::shared::user::responses::field(&form, "action") == "recover-owner" {
            crate::organizations::organization::draft(&form)
        } else {
            Fields::new()
        };
    let ids = if crate::organizations::organization::bounded(&form)
        && codeyard_api_http::shared::user::responses::field(&form, "action") == "recover-owner"
    {
        crate::organizations::organization::id(&form, "org_id")
            .ok()
            .zip(crate::organizations::organization::username(&form).ok())
    } else {
        None
    };
    let Some((org, username)) = ids else {
        return crate::organizations::organization::respond(
            &ctx,
            None,
            StatusCode::BAD_REQUEST,
            Some("Choose an organization and enter the new owner's username.".into()),
            draft,
        )
        .await;
    };
    // The typed name must match the organization being recovered, and only ownerless ones are offered.
    let ownerless = match list_ownerless_organizations(
        &ctx.state.pool,
        actor,
        codeyard_db_orgs::organizations::OrganizationPage::new(org.saturating_sub(1), 1)
            .expect("positive organization id"),
    )
    .await
    {
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
        return crate::organizations::organization::respond(
            &ctx,
            None,
            StatusCode::CONFLICT,
            Some("Recovery was not applied. Confirm suspended governance and an eligible current member.".into()),
            draft,
        )
        .await;
    };
    if codeyard_api_http::shared::user::responses::field(&form, "confirm") != recovered.name {
        return crate::organizations::organization::respond(
            &ctx,
            None,
            StatusCode::BAD_REQUEST,
            Some("Type the organization name to confirm recovery.".into()),
            draft,
        )
        .await;
    }
    let target = match crate::organizations::membership::target(&ctx, &username).await {
        Ok(Some(target)) => target,
        Ok(None) => {
            return crate::organizations::organization::respond(
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
    let (status, message) = match recover_suspended_organization_owner(&ctx.state.pool, (actor, codeyard_api_http::session::extract_session_id(&ctx.headers).unwrap_or_default()), org, target).await {
        Ok(OrganizationOutcome::Applied(_)) => return ctx.redirect(INDEX),
        Ok(_) => (StatusCode::CONFLICT, "Recovery was not applied. Confirm suspended governance and an eligible current member."),
        Err(_) => (StatusCode::SERVICE_UNAVAILABLE, "Organization recovery is temporarily unavailable; no success was confirmed."),
    };
    crate::organizations::organization::respond(&ctx, None, status, Some(message.into()), draft)
        .await
}
