//! Gate requests are private to the App whose decision the run is waiting for.
use super::{enqueue, events, outgoing, payloads};
use crate::backend::api::github::repo_url;
use crate::backend::db;
use crate::backend::identity::github_apps;
use codeyard_db_core::models::CiRun;
use codeyard_db_core::DbPool;
use serde_json::{json, Value};

pub async fn requested(pool: &DbPool, run: &CiRun) -> anyhow::Result<()> {
    let gates = db::deployment_protection::gates(pool, run.id).await?;
    if !gates.iter().any(|gate| gate.state == "pending") {
        return Ok(());
    }
    let Some(repo) = db::repositories::records::get_repository_by_id(pool, run.repo_id).await?
    else {
        return Ok(());
    };
    let mut owners = db::repositories::records::repository_owner_names(pool, &[repo.id]).await?;
    let Some(owner) = owners.remove(&repo.id) else {
        return Ok(());
    };
    let Some(deployment) = db::ci::get_ci_deployment_by_run_id(pool, run.id).await? else {
        return Ok(());
    };
    let sender = if let Some(actor) = run.triggered_by_id {
        db::users::accounts::get_user_by_id(pool, actor)
            .await?
            .map(|user| payloads::user(&user))
            .unwrap_or(Value::Null)
    } else {
        Value::Null
    };
    // The run's pull request, when it belongs to this repository. A fork's head repository is
    // left out of the document, as in every pull request payload.
    let mut pull_requests = Vec::new();
    if let Some(id) = run.pull_request_id {
        if let Some(pull) = db::pull_requests::records::get_pull_request_by_id(pool, id)
            .await?
            .filter(|pull| pull.repo_id == repo.id)
        {
            let author = db::users::accounts::get_user_by_id(pool, pull.author_id)
                .await?
                .map(|user| payloads::Person::of(&user))
                .unwrap_or(payloads::Person {
                    login: String::new(),
                    id: pull.author_id,
                    site_admin: false,
                });
            pull_requests.push(payloads::pull_request_json(&pull, &author, &repo, &owner));
        }
    }
    let base = repo_url(&owner, &repo.name);
    let callback = format!(
        "{base}/actions/runs/{}/deployment_protection_rule",
        run.workflow_run_id.unwrap_or(run.id)
    );
    for subscription in db::github_app_hooks::for_repository(pool, repo.id).await? {
        if !gates.iter().any(|gate| {
            gate.app_id == subscription.hook.app_id.unwrap_or(0) && gate.state == "pending"
        }) || !events::subscribes(
            &subscription.hook.events,
            events::DEPLOYMENT_PROTECTION_RULE,
        ) || !github_apps::event_permission(
            &serde_json::from_str(&subscription.permissions)?,
            events::DEPLOYMENT_PROTECTION_RULE,
        ) {
            continue;
        }
        // The CI deployment has no REST deployment row behind it, so its id must not be used in
        // deployment URLs. The commit is sent at the top level, and the deployment is null.
        let payload = json!({
            "action":"requested", "environment":deployment.environment, "event":run.event,
            "deployment_callback_url":callback, "run_id":run.id,
            "deployment":null, "sha":run.head_sha, "ref":run.branch,
            "pull_requests":pull_requests, "repository":payloads::repository(&repo,&owner), "sender":sender,
            "installation":{"id":subscription.installation_id}
        });
        let outgoing = outgoing(
            events::DEPLOYMENT_PROTECTION_RULE,
            Some("requested"),
            &subscription.hook,
            &payload.to_string(),
        );
        if !enqueue(pool, subscription.hook, outgoing) {
            tracing::warn!(
                run_id = run.id,
                "Deployment protection webhook queue is full"
            );
        }
    }
    Ok(())
}
