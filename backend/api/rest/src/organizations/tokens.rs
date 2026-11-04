//! Owners set the organization's token policy and review the fine-grained tokens that reach
//! it. The same rows back the REST personal-access-token endpoints.

use crate::organizations::organization::Fields;
use axum::extract::rejection::FormRejection;
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
use codeyard_db_core::models::OrgRole;
use codeyard_db_core::queries::fine_grained_tokens::Policy;
use codeyard_db_core::queries::organizations::OrganizationOutcome;
use codeyard_identity::fine_grained::MAX_DAYS;
use codeyard_model::fine_grained::catalog;
use codeyard_webui::pages::orgs as view;
use serde_json::Map;
use serde_json::Value;
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;

pub(super) fn router() -> Router<Arc<AppState>> {
    Router::new().route(
        "/settings/organizations/:id/personal-access-tokens",
        post(mutate),
    )
}

#[expect(
    clippy::result_large_err,
    reason = "HTML adapters preserve complete Axum responses under the PageContext contract."
)]
async fn require_owner(ctx: &PageContext, actor: i64, org: i64) -> Result<(), Response> {
    match super::get_organization_role(&ctx.state.pool, actor, org).await {
        Ok(OrganizationOutcome::Applied(OrgRole::Owner)) => Ok(()),
        Ok(OrganizationOutcome::Applied(_)) => Err(ctx.error(
            StatusCode::FORBIDDEN,
            "Only organization owners can manage personal access tokens.",
        )),
        Ok(_) => Err(crate::organizations::organization::not_found(ctx)),
        Err(_) => Err(crate::organizations::organization::unavailable(ctx)),
    }
}

/// Projects listed tokens with their users and selected repository names.
async fn grants(
    ctx: &PageContext,
    rows: Vec<codeyard_db_core::queries::fine_grained_tokens::Token>,
    repositories: &HashMap<i64, Vec<String>>,
) -> anyhow::Result<Vec<view::TokenGrant>> {
    let mut ids: Vec<_> = rows.iter().map(|token| token.user_id).collect();
    ids.sort_unstable();
    ids.dedup();
    let users: HashMap<_, _> =
        codeyard_db_identity::users::accounts::get_users_by_ids(&ctx.state.pool, &ids)
            .await?
            .into_iter()
            .map(|user| (user.id, user.username))
            .collect();
    Ok(rows
        .into_iter()
        .map(|token| {
            let stored: Map<String, Value> =
                serde_json::from_str(&token.permissions).unwrap_or_default();
            let permissions = stored
                .iter()
                .filter_map(|(key, level)| {
                    Some(format!("{}: {}", catalog(key)?.name, level.as_str()?))
                })
                .collect();
            view::TokenGrant {
                id: token.token_id,
                user: users.get(&token.user_id).cloned().unwrap_or_default(),
                name: token.name,
                description: token.description,
                repositories: repositories
                    .get(&token.token_id)
                    .cloned()
                    .unwrap_or_default(),
                access: token.repository_access,
                permissions,
                requested: token.requested_at.to_rfc3339(),
                expires: token
                    .expires_at
                    .map_or_else(|| "never".to_owned(), |at| at.to_rfc3339()),
                last_used: token.last_used_at.map(|at| at.to_rfc3339()),
            }
        })
        .collect())
}

/// The owners' tab. The owner check, policy, lists and repository names come from one
/// snapshot, so a demoted owner sees nothing created after the demotion.
pub(super) async fn panel(
    ctx: &PageContext,
    actor: i64,
    org: i64,
) -> Result<view::TokenPanel, Response> {
    let unavailable = |_| crate::organizations::organization::unavailable(ctx);
    let limit = crate::organizations::organization::PAGE_LIMIT;
    let (Some((pending_before, _)), Some((active_before, _))) = (
        crate::organizations::organization::cursor(ctx, "pending_before"),
        crate::organizations::organization::cursor(ctx, "active_before"),
    ) else {
        return Err(crate::organizations::organization::invalid(ctx));
    };
    let loaded = codeyard_db_identity::fine_grained_tokens::owner_tokens(
        &ctx.state.pool,
        actor,
        ctx.session_id.as_deref().unwrap_or(""),
        org,
        (pending_before, active_before),
        limit + 1,
    )
    .await
    .map_err(unavailable)?
    .ok_or_else(|| crate::organizations::organization::not_found(ctx))?;
    let mut repositories: HashMap<i64, Vec<String>> = HashMap::new();
    for (token, name) in loaded.repositories {
        repositories.entry(token).or_default().push(name);
    }
    let base = format!(
        "{}/{org}?tab=personal-access-tokens",
        crate::organizations::INDEX
    );
    let (mut pending, mut active) = (loaded.pending, loaded.approved);
    let pending_next = (pending.len() as i64 > limit).then(|| {
        pending.truncate(limit as usize);
        let last = pending.last().map_or(0, |token| token.token_id);
        format!("{base}&pending_before={last}&active_before={active_before}")
    });
    let active_next = (active.len() as i64 > limit).then(|| {
        active.truncate(limit as usize);
        let last = active.last().map_or(0, |token| token.token_id);
        format!("{base}&pending_before={pending_before}&active_before={last}")
    });
    let policy = loaded.policy;
    Ok(view::TokenPanel {
        fine_grained_allowed: policy.fine_grained_allowed,
        fine_grained_approval: policy.fine_grained_approval,
        classic_allowed: policy.classic_allowed,
        max_lifetime_days: policy
            .max_lifetime_days
            .map(|days| days.to_string())
            .unwrap_or_default(),
        pending: grants(ctx, pending, &repositories)
            .await
            .map_err(unavailable)?,
        active: grants(ctx, active, &repositories)
            .await
            .map_err(unavailable)?,
        pending_next,
        active_next,
    })
}

enum Outcome {
    Saved,
    Changed,
    Invalid(&'static str),
}

fn policy(form: &Fields) -> Result<Policy, &'static str> {
    let flag = |key: &str| match form.get(key).map(String::as_str) {
        None | Some("false") => Ok(false),
        Some("true") => Ok(true),
        _ => Err("Use the token policy checkboxes."),
    };
    let days =
        match codeyard_api_http::shared::user::responses::field(form, "max_lifetime_days").trim() {
            "" => None,
            value => Some(
                value
                    .parse::<i32>()
                    .ok()
                    .filter(|days| (1..=MAX_DAYS as i32).contains(days))
                    .ok_or("Use a maximum lifetime of 1 to 366 days, or leave it empty.")?,
            ),
        };
    Ok(Policy {
        fine_grained_allowed: flag("fine_grained_allowed")?,
        fine_grained_approval: flag("fine_grained_approval")?,
        classic_allowed: flag("classic_allowed")?,
        max_lifetime_days: days,
    })
}

async fn apply(ctx: &PageContext, actor: i64, org: i64, form: &Fields) -> anyhow::Result<Outcome> {
    let actor = codeyard_db_core::queries::credentials::Actor::from((
        actor,
        codeyard_api_http::session::extract_session_id(&ctx.headers).unwrap_or_default(),
    ));
    let pool = &ctx.state.pool;
    let token = codeyard_api_http::shared::user::responses::field(form, "token_id")
        .parse::<i64>()
        .ok();
    let changed = |done: bool| {
        if done {
            Outcome::Saved
        } else {
            Outcome::Changed
        }
    };
    Ok(
        match (
            codeyard_api_http::shared::user::responses::field(form, "action"),
            token,
        ) {
            ("token-policy", _) => match policy(form) {
                Ok(policy) => changed(
                    codeyard_db_identity::fine_grained_tokens::set_policy(
                        pool, actor, org, &policy,
                    )
                    .await?,
                ),
                Err(message) => Outcome::Invalid(message),
            },
            ("token-review", Some(token)) => {
                let status =
                    match codeyard_api_http::shared::user::responses::field(form, "decision") {
                        "approve" => "approved",
                        "deny" => "denied",
                        _ => return Ok(Outcome::Invalid("Approve or deny the request.")),
                    };
                let reason =
                    crate::organizations::organization::optional(form, "reason").map(str::trim);
                if reason.is_some_and(|reason| reason.len() > 1024 || reason.contains('\0')) {
                    return Ok(Outcome::Invalid("Use a reason of at most 1024 characters."));
                }
                changed(
                    codeyard_db_identity::fine_grained_tokens::review(
                        pool,
                        org,
                        actor,
                        &[token],
                        "pending",
                        status,
                        reason,
                    )
                    .await?
                        == 1,
                )
            }
            ("token-revoke", Some(token)) => changed(
                codeyard_db_identity::fine_grained_tokens::review(
                    pool,
                    org,
                    actor,
                    &[token],
                    "approved",
                    "revoked",
                    None,
                )
                .await?
                    == 1,
            ),
            _ => Outcome::Invalid(crate::organizations::organization::INVALID_FORM),
        },
    )
}

async fn mutate(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: Uri,
    Path(org): Path<i64>,
    input: Result<Form<Vec<(String, String)>>, FormRejection>,
) -> Response {
    let mut ctx = match PageContext::load(state, headers, &uri).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    ctx.query
        .insert("tab".into(), "personal-access-tokens".into());
    let Ok(Form(pairs)) = input else {
        return crate::organizations::organization::invalid(&ctx);
    };
    let mut seen = HashSet::new();
    if pairs.len() > 12
        || pairs
            .iter()
            .any(|(key, value)| key.len() > 64 || value.len() > 4096 || !seen.insert(key))
    {
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
    if let Err(response) = crate::organizations::organization::selected(&ctx, actor, org).await {
        return response;
    }
    if let Err(response) = require_owner(&ctx, actor, org).await {
        return response;
    }
    let (status, message) = match apply(&ctx, actor, org, &form).await {
        Ok(Outcome::Saved) => {
            return ctx.redirect(&format!(
                "{}/{org}?tab=personal-access-tokens",
                crate::organizations::INDEX
            ))
        }
        Ok(Outcome::Changed) => (
            StatusCode::CONFLICT,
            "The token or request changed. Reload and try again.",
        ),
        Ok(Outcome::Invalid(message)) => (StatusCode::BAD_REQUEST, message),
        Err(_) => return crate::organizations::organization::unavailable(&ctx),
    };
    let draft = form
        .into_iter()
        .filter(|(key, _)| key != "csrf_token")
        .collect();
    crate::organizations::organization::respond(
        &ctx,
        Some(org),
        status,
        Some(message.into()),
        draft,
    )
    .await
}
