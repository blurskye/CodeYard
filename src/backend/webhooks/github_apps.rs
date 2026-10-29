//! App subscriptions use the native payload builder, sender, URL safety and delivery queue.
use super::{
    delivery::{self, Outgoing},
    enqueue, events, outgoing,
};
use crate::backend::{
    db::{
        self,
        models::{Repository, Webhook},
        DbPool,
    },
    identity::github_apps as apps,
};
use serde_json::{json, Value};

fn accepts(subscription: &db::github_app_hooks::Subscription, event: &str) -> anyhow::Result<bool> {
    let permissions: Value = serde_json::from_str(&subscription.permissions)?;
    Ok(subscription.hook.active && events::subscribes(&subscription.hook.events, event)
        && apps::event_permission(&permissions, event))
}

pub(super) async fn has_subscription(pool: &DbPool, repo: i64, event: &str) -> anyhow::Result<bool> {
    for subscription in db::github_app_hooks::for_repository(pool, repo).await? {
        if accepts(&subscription, event)? { return Ok(true); }
    }
    Ok(false)
}

pub async fn emit_repository(
    pool: &DbPool,
    repo: &Repository,
    event: &str,
    action: Option<&str>,
    guid: &str,
    payload: &Value,
) -> anyhow::Result<()> {
    for subscription in db::github_app_hooks::for_repository(pool, repo.id).await? {
        if !accepts(&subscription, event)? {
            continue;
        }
        let mut payload = payload.clone();
        payload["installation"] = json!({"id":subscription.installation_id});
        let mut outgoing = outgoing(event, action, &subscription.hook, &payload.to_string());
        outgoing.guid = guid.to_owned();
        if !enqueue(pool, subscription.hook, outgoing) {
            tracing::warn!(event, "GitHub App webhook queue is full");
        }
    }
    Ok(())
}
/// Recheck after queueing and before each network attempt, including installation membership.
pub async fn current(
    pool: &DbPool,
    app: i64,
    hook: &Webhook,
    outgoing: &Outgoing,
) -> anyhow::Result<bool> {
    if db::github_apps::app(pool, &app.to_string())
        .await?
        .is_none()
    {
        return Ok(false);
    }
    let Some(current) = db::github_app_hooks::hook(pool, app).await? else {
        return Ok(false);
    };
    if current.payload_url != hook.payload_url
        || current.content_type != hook.content_type
        || current.encrypted_secret != hook.encrypted_secret
        || current.secret_nonce != hook.secret_nonce
        || current.ssl_verify != hook.ssl_verify
    {
        return Ok(false);
    }
    if outgoing.automatic && (!current.active || !events::subscribes(&current.events, &outgoing.event)) {
        return Ok(false);
    }
    let Some(payload) = delivery::json_payload(outgoing) else {
        return Ok(false);
    };
    let (Some(repo), Some(installation)) = (
        payload["repository"]["id"].as_i64(),
        payload["installation"]["id"].as_i64(),
    ) else {
        return Ok(outgoing.event == events::PING && payload.get("installation").is_none());
    };
    if outgoing.event == events::DEPLOYMENT_PROTECTION_RULE {
        let Some(run) = payload["run_id"].as_i64() else {
            return Ok(false);
        };
        if !db::deployment_protection::gates(pool, run)
            .await?
            .iter()
            .any(|gate| gate.app_id == app && gate.state == "pending")
        {
            return Ok(false);
        }
    }
    let Some(subscription) = db::github_app_hooks::subscription(pool, repo, app, installation, outgoing.automatic).await? else {
        return Ok(false);
    };
    Ok(apps::event_permission(&serde_json::from_str(&subscription.permissions)?, &outgoing.event))
}
