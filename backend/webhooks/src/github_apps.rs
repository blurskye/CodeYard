//! App subscriptions use the native payload builder, sender, URL safety and delivery queue.

use crate::delivery::Outgoing;
use crate::enqueue;
use crate::outgoing;
use codeyard_db_core::models::Repository;
use codeyard_db_core::models::Webhook;
use codeyard_db_core::DbPool;
use serde_json::json;
use serde_json::Value;

fn targets_app(event: &str, action: Option<&str>) -> bool {
    matches!(
        (event, action),
        (
            codeyard_model::webhook_events::CHECK_SUITE,
            Some("requested" | "rerequested")
        ) | (
            codeyard_model::webhook_events::CHECK_RUN,
            Some("rerequested" | "requested_action")
        )
    )
}

fn accepts(
    subscription: &codeyard_db_identity::github_app_hooks::Subscription,
    event: &str,
) -> anyhow::Result<bool> {
    let permissions: Value = serde_json::from_str(&subscription.permissions)?;
    Ok(subscription.hook.active
        && codeyard_model::webhook_events::subscribes(&subscription.hook.events, event)
        && codeyard_identity::github_apps::event_permission(&permissions, event))
}

pub(super) async fn has_subscription(
    pool: &DbPool,
    repo: i64,
    event: &str,
) -> anyhow::Result<bool> {
    for subscription in codeyard_db_identity::github_app_hooks::for_repository(pool, repo).await? {
        if accepts(&subscription, event)? {
            return Ok(true);
        }
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
    let subscriptions = if targets_app(event, action) {
        let Some(client) = payload[event]["app"]["client_id"].as_str() else {
            return Ok(());
        };
        let Some(app) = codeyard_db_identity::github_apps::app(pool, client)
            .await?
            .filter(|app| app.client_id == client)
        else {
            return Ok(());
        };
        codeyard_db_identity::github_app_hooks::for_repository_app(pool, repo.id, app.id).await?
    } else {
        codeyard_db_identity::github_app_hooks::for_repository(pool, repo.id).await?
    };
    for subscription in subscriptions {
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
    let Some(application) = codeyard_db_identity::github_apps::app(pool, &app.to_string())
        .await?
        .filter(|row| row.id == app)
    else {
        return Ok(false);
    };
    let Some(current) = codeyard_db_identity::github_app_hooks::hook(pool, app).await? else {
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
    if outgoing.automatic
        && (!current.active
            || !codeyard_model::webhook_events::subscribes(&current.events, &outgoing.event))
    {
        return Ok(false);
    }
    let Some(payload) = crate::delivery::json_payload(outgoing) else {
        return Ok(false);
    };
    if targets_app(&outgoing.event, payload["action"].as_str())
        && payload[outgoing.event.as_str()]["app"]["client_id"].as_str()
            != Some(application.client_id.as_str())
    {
        return Ok(false);
    }
    let (Some(repo), Some(installation)) = (
        payload["repository"]["id"].as_i64(),
        payload["installation"]["id"].as_i64(),
    ) else {
        return Ok(outgoing.event == codeyard_model::webhook_events::PING
            && payload.get("installation").is_none());
    };
    if outgoing.event == codeyard_model::webhook_events::DEPLOYMENT_PROTECTION_RULE {
        let Some(run) = payload["run_id"].as_i64() else {
            return Ok(false);
        };
        if !codeyard_db_actions::deployment_protection::gates(pool, run)
            .await?
            .iter()
            .any(|gate| gate.app_id == app && gate.state == "pending")
        {
            return Ok(false);
        }
    }
    let Some(subscription) = codeyard_db_identity::github_app_hooks::subscription(
        pool,
        repo,
        app,
        installation,
        outgoing.automatic,
    )
    .await?
    else {
        return Ok(false);
    };
    Ok(codeyard_identity::github_apps::event_permission(
        &serde_json::from_str(&subscription.permissions)?,
        &outgoing.event,
    ))
}
