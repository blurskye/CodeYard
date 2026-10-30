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

/// Projects listed tokens with their users and selected repository names.
async fn grants(
    ctx: &PageContext,
    rows: Vec<db::fine_grained_tokens::Token>,
    repositories: &HashMap<i64, Vec<String>>,
) -> anyhow::Result<Vec<view::TokenGrant>> {
    let mut ids: Vec<_> = rows.iter().map(|token| token.user_id).collect();
    ids.sort_unstable();
    ids.dedup();
    let users: HashMap<_, _> = db::users::accounts::get_users_by_ids(&ctx.state.pool, &ids)
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
    let unavailable = |_| organization::unavailable(ctx);
    let limit = organization::PAGE_LIMIT;
    let (Some((pending_before, _)), Some((active_before, _))) = (
        organization::cursor(ctx, "pending_before"),
        organization::cursor(ctx, "active_before"),
    ) else {
        return Err(organization::invalid(ctx));
    };
    let loaded = db::fine_grained_tokens::owner_tokens(
        &ctx.state.pool,
        actor,
        ctx.session_id.as_deref().unwrap_or(""),
        org,
        (pending_before, active_before),
        limit + 1,
    )
    .await
    .map_err(unavailable)?
    .ok_or_else(|| organization::not_found(ctx))?;
    let mut repositories: HashMap<i64, Vec<String>> = HashMap::new();
    for (token, name) in loaded.repositories {
        repositories.entry(token).or_default().push(name);
    }
    let base = format!("{}/{org}?tab=personal-access-tokens", super::INDEX);
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
