//! Owners set the organization's token policy and review the fine-grained tokens that reach
//! it. The same rows back the REST personal-access-token endpoints.
use super::organization::{self, Fields};
use crate::backend::{
    api::http::context::PageContext,
    app::AppState,
    db::{
        self,
        fine_grained_tokens::Policy,
        models::OrgRole,
        organizations::{self, OrganizationOutcome},
    },
    identity::fine_grained::{catalog, MAX_DAYS},
};
use crate::webui::pages::orgs as view;
use axum::{
    extract::{rejection::FormRejection, Form, Path, State},
    http::{HeaderMap, StatusCode, Uri},
    response::Response,
    routing::post,
    Router,
};
use serde_json::{Map, Value};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

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
    match organizations::get_organization_role(&ctx.state.pool, actor, org).await {
        Ok(OrganizationOutcome::Applied(OrgRole::Owner)) => Ok(()),
        Ok(OrganizationOutcome::Applied(_)) => Err(ctx.error(
            StatusCode::FORBIDDEN,
            "Only organization owners can manage personal access tokens.",
        )),
        Ok(_) => Err(organization::not_found(ctx)),
        Err(_) => Err(organization::unavailable(ctx)),
    }
}

/// The organization's tokens in `status`, newest first, at most 100.
async fn grants(
    ctx: &PageContext,
    org: i64,
    status: &str,
) -> anyhow::Result<Vec<view::TokenGrant>> {
    let pool = &ctx.state.pool;
    let mut rows = db::fine_grained_tokens::organization_tokens(pool, org, status).await?;
    rows.truncate(100);
    let mut ids: Vec<_> = rows.iter().map(|token| token.user_id).collect();
    ids.sort_unstable();
    ids.dedup();
    let users: HashMap<_, _> = db::users::accounts::get_users_by_ids(pool, &ids)
        .await?
        .into_iter()
        .map(|user| (user.id, user.username))
        .collect();
    let mut grants = Vec::new();
    for token in rows {
        let repositories = if token.repository_access == "selected" {
            db::fine_grained_tokens::repositories(pool, token.token_id)
                .await?
                .into_iter()
                .map(|(_, name)| name)
                .collect()
        } else {
            Vec::new()
        };
        let stored: Map<String, Value> =
            serde_json::from_str(&token.permissions).unwrap_or_default();
        let permissions = stored
            .iter()
            .filter_map(|(key, level)| Some(format!("{}: {}", catalog(key)?.name, level.as_str()?)))
            .collect();
        grants.push(view::TokenGrant {
            id: token.token_id,
            user: users.get(&token.user_id).cloned().unwrap_or_default(),
            name: token.name,
            description: token.description,
            access: token.repository_access,
            repositories,
            permissions,
            requested: token.requested_at.to_rfc3339(),
            expires: token
                .expires_at
                .map_or_else(|| "never".to_owned(), |at| at.to_rfc3339()),
            last_used: token.last_used_at.map(|at| at.to_rfc3339()),
        });
    }
    Ok(grants)
}

/// The owners' tab. The organization loader shows it to owners only.
pub(super) async fn panel(ctx: &PageContext, org: i64) -> Result<view::TokenPanel, Response> {
    let unavailable = |_| organization::unavailable(ctx);
    let policy = db::fine_grained_tokens::policy(&ctx.state.pool, org)
        .await
        .map_err(unavailable)?;
    Ok(view::TokenPanel {
        fine_grained_allowed: policy.fine_grained_allowed,
        fine_grained_approval: policy.fine_grained_approval,
        classic_allowed: policy.classic_allowed,
        max_lifetime_days: policy
            .max_lifetime_days
            .map(|days| days.to_string())
            .unwrap_or_default(),
        pending: grants(ctx, org, "pending").await.map_err(unavailable)?,
        active: grants(ctx, org, "approved").await.map_err(unavailable)?,
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
    let days = match organization::field(form, "max_lifetime_days").trim() {
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
    let pool = &ctx.state.pool;
    let token = organization::field(form, "token_id").parse::<i64>().ok();
    let changed = |done: bool| {
        if done {
            Outcome::Saved
        } else {
            Outcome::Changed
        }
    };
    Ok(match (organization::field(form, "action"), token) {
        ("token-policy", _) => match policy(form) {
            Ok(policy) => {
                changed(db::fine_grained_tokens::set_policy(pool, actor, org, &policy).await?)
            }
            Err(message) => Outcome::Invalid(message),
        },
        ("token-review", Some(token)) => {
            let status = match organization::field(form, "decision") {
                "approve" => "approved",
                "deny" => "denied",
                _ => return Ok(Outcome::Invalid("Approve or deny the request.")),
            };
            let reason = organization::optional(form, "reason").map(str::trim);
            if reason.is_some_and(|reason| reason.len() > 1024 || reason.contains('\0')) {
                return Ok(Outcome::Invalid("Use a reason of at most 1024 characters."));
            }
            changed(
                db::fine_grained_tokens::review(
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
            db::fine_grained_tokens::review(
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
        _ => Outcome::Invalid(organization::INVALID_FORM),
    })
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
        return organization::invalid(&ctx);
    };
    let mut seen = HashSet::new();
    if pairs.len() > 12
        || pairs
            .iter()
            .any(|(key, value)| key.len() > 64 || value.len() > 4096 || !seen.insert(key))
    {
        return organization::invalid(&ctx);
    }
    let form: Fields = pairs.into_iter().collect();
    if let Err(response) = ctx.check_csrf(&form) {
        return response;
    }
    let actor = match ctx.require_user() {
        Ok(user) => user.id,
        Err(response) => return response,
    };
    if let Err(response) = organization::selected(&ctx, actor, org).await {
        return response;
    }
    if let Err(response) = require_owner(&ctx, actor, org).await {
        return response;
    }
    let (status, message) = match apply(&ctx, actor, org, &form).await {
        Ok(Outcome::Saved) => {
            return ctx.redirect(&format!(
                "{}/{org}?tab=personal-access-tokens",
                super::INDEX
            ))
        }
        Ok(Outcome::Changed) => (
            StatusCode::CONFLICT,
            "The token or request changed. Reload and try again.",
        ),
        Ok(Outcome::Invalid(message)) => (StatusCode::BAD_REQUEST, message),
        Err(_) => return organization::unavailable(&ctx),
    };
    let draft = form
        .into_iter()
        .filter(|(key, _)| key != "csrf_token")
        .collect();
    organization::respond(&ctx, Some(org), status, Some(message.into()), draft).await
}
