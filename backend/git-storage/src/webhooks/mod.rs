//! Webhooks: subscriptions on repositories and organizations, and the background sends that
//! follow a committed write. Callers never wait for a send, and a send never fails a request.
use codeyard_database::models::Repository;
use codeyard_database::models::Webhook;
use codeyard_database::DbPool;
use codeyard_db_repos::webhooks::HookOwner;
use codeyard_types::webhook_events as events;
use codeyard_webhooks::payloads::Person;
use serde_json::Value;
use std::collections::HashMap;
pub mod documents;

pub mod ref_changes;
pub mod statuses;

/// The users a payload names, looked up only when some hook wants the event.
#[derive(Debug, Default)]
pub struct People(HashMap<i64, Person>);

impl People {
    /// A user that is missing shows an empty login instead of failing the payload.
    pub fn get(&self, id: i64) -> Person {
        self.0.get(&id).cloned().unwrap_or(Person {
            login: String::new(),
            id,
            site_admin: false,
        })
    }
}

/// Records activity after the native write, whether or not hooks exist. Building is bounded;
/// network deliveries remain background work and cannot prevent event persistence.
pub async fn emit<F>(
    pool: &DbPool,
    repo: &Repository,
    event: &'static str,
    user_ids: Vec<i64>,
    build: F,
) -> Value
where
    F: FnOnce(&str, &People) -> Value + Send + 'static,
{
    emit_checked(pool, repo, event, user_ids, None, build).await
}

pub async fn emit_repository_dispatch(
    pool: &DbPool,
    repo: &Repository,
    actor: i64,
    sender: i64,
    event_type: String,
    client_payload: Value,
    credential: codeyard_database::queries::ci::CiCredential,
) -> Value {
    let snapshot = repo.clone();
    emit_checked(
        pool,
        repo,
        "repository_dispatch",
        vec![sender],
        Some((actor, credential)),
        move |owner, people| {
            serde_json::json!({"action":event_type, "branch":snapshot.default_branch,
            "client_payload":client_payload, "repository":payloads::repository(&snapshot, owner),
            "sender":payloads::person(&people.get(sender))})
        },
    )
    .await
}

async fn emit_checked<F>(
    pool: &DbPool,
    repo: &Repository,
    event: &'static str,
    user_ids: Vec<i64>,
    dispatch_actor: Option<(i64, codeyard_database::queries::ci::CiCredential)>,
    build: F,
) -> Value
where
    F: FnOnce(&str, &People) -> Value + Send + 'static,
{
    let Ok(Some(current)) =
        codeyard_db_repos::repositories::records::get_repository_by_id(pool, repo.id).await
    else {
        return Value::Null;
    };
    // Never select a former organization's hooks using a pre-transfer snapshot.
    if !same_owner(&current, repo) {
        return Value::Null;
    }
    // Job transitions have no activity record and their caller does not use a document.
    // Include App subscriptions before deciding there is no work to project.
    let early_hooks = if event == "workflow_job" {
        let hooks = match hooks_for(pool, &current).await {
            Ok(hooks) => hooks,
            Err(error) => {
                tracing::warn!(repo_id=repo.id, %error, "could not load job webhook subscriptions");
                return Value::Null;
            }
        };
        if !hooks
            .iter()
            .any(|hook| hook.active && events::subscribes(&hook.events, event))
        {
            match codeyard_webhooks::github_apps::has_subscription(pool, repo.id, event).await {
                Ok(true) => {}
                Ok(false) => return Value::Null,
                Err(error) => {
                    tracing::warn!(repo_id=repo.id, %error, "could not load App job webhook subscriptions");
                    return Value::Null;
                }
            }
        }
        Some(hooks)
    } else {
        None
    };
    let Ok(build_slot) = BUILD_SLOTS.clone().acquire_owned().await else {
        return Value::Null;
    };
    let owner = codeyard_db_repos::repositories::records::repository_owner_names(pool, &[repo.id])
        .await
        .ok()
        .and_then(|mut names| names.remove(&repo.id));
    let Some(owner) = owner else {
        return Value::Null;
    };
    let people = people(pool, user_ids).await;
    let builder_owner = owner.clone();
    let (mut payload, _build_slot) =
        match tokio::task::spawn_blocking(move || (build(&builder_owner, &people), build_slot))
            .await
        {
            Ok(value) => value,
            Err(error) => {
                tracing::warn!(repo_id=repo.id,%error,"could not build activity payload");
                return Value::Null;
            }
        };
    if !payload.is_object() {
        return Value::Null;
    }
    if let Some(org) = current.owner_org_id {
        match codeyard_db_orgs::organizations::profiles::get_public_organization_by_id(pool, org)
            .await
        {
            Ok(Some(org)) => {
                payload["organization"] = codeyard_webhooks::payloads::organization(&org)
            }
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(repo_id=repo.id, %error, "could not load webhook organization")
            }
        }
    }
    set_open_issues(pool, repo.id, &mut payload).await;
    crate::webhooks::documents::complete(pool, &current, &owner, &mut payload).await;
    // Enrichment reads the stored issue or pull request as it is now. If the repository changed
    // owner meanwhile, those reads may hold the new owner's content, so the emission stops.
    if !matches!(
        codeyard_db_repos::repositories::records::get_repository_by_id(pool, repo.id).await,
        Ok(Some(now)) if same_owner(&now, repo)
    ) {
        return Value::Null;
    }
    // Dispatch is a requested write, not notification of a previously committed write.
    // Accept it only after all bounded preparation, under the mutation authority lock.
    if let Some((actor, credential)) = dispatch_actor {
        if !matches!(
            codeyard_db_actions::ci::accept_repository_dispatch(pool, actor, repo.id, &credential)
                .await,
            Ok(true)
        ) {
            return Value::Null;
        }
    }
    if let Err(error) = record_event(
        pool,
        repo,
        event,
        &payload,
        !current.is_private && !repo.is_private,
    )
    .await
    {
        tracing::warn!(repo_id=repo.id,%error,"could not record repository activity");
    }
    let json = payload.to_string();
    // The limit is on deliveries; the event itself, and the workflows it starts, go ahead.
    if json.len() > MAX_PAYLOAD_BYTES {
        tracing::warn!(event, "webhook payload exceeds its size limit");
        return payload;
    }
    if TASKS.is_closed() {
        return payload;
    }
    let hooks = match early_hooks {
        Some(hooks) => hooks,
        None => match hooks_for(pool, repo).await {
            Ok(hooks) => hooks,
            Err(error) => {
                tracing::warn!(repo_id=repo.id,%error,"could not load webhooks");
                return payload;
            }
        },
    };
    let action = payload.get("action").and_then(Value::as_str);
    // One event has one delivery GUID, shared by every hook that receives it.
    let guid = uuid::Uuid::new_v4().to_string();
    if let Err(error) =
        codeyard_webhooks::github_apps::emit_repository(pool, repo, event, action, &guid, &payload)
            .await
    {
        tracing::warn!(repo_id=repo.id,%error,"could not queue GitHub App webhooks");
    }
    for hook in hooks
        .into_iter()
        .filter(|h| h.active && events::subscribes(&h.events, event))
    {
        let mut outgoing = outgoing(event, action, &hook, &json);
        outgoing.guid.clone_from(&guid);
        if !enqueue_delivery(pool, hook, outgoing) {
            tracing::warn!(event, "webhook queue is full; a delivery was dropped");
        }
    }
    payload
}

/// Whether the repository has the owners an emission was made for.
fn same_owner(current: &Repository, repo: &Repository) -> bool {
    current.owner_user_id == repo.owner_user_id && current.owner_org_id == repo.owner_org_id
}

async fn record_event(
    pool: &DbPool,
    repo: &Repository,
    event: &str,
    payload: &Value,
    public: bool,
) -> anyhow::Result<()> {
    // GitHub's activity shows a new repository as a CreateEvent, though its webhook is
    // `repository`.
    let created;
    let (event, payload) = if event == events::REPOSITORY && payload["action"] == "created" {
        created = serde_json::json!({
            "ref": null,
            "ref_type": "repository",
            "full_ref": null,
            "master_branch": repo.default_branch,
            "description": repo.description,
            "pusher_type": "user",
            "sender": payload["sender"],
        });
        (events::CREATE, &created)
    } else {
        (event, payload)
    };
    let kind = match event {
        events::PUSH => "PushEvent",
        events::CREATE => "CreateEvent",
        events::DELETE => "DeleteEvent",
        events::FORK => "ForkEvent",
        events::STAR if payload["action"] == "created" => "WatchEvent",
        events::RELEASE
            if payload["release"]["draft"] == false && payload["action"] == "published" =>
        {
            "ReleaseEvent"
        }
        events::COMMIT_COMMENT => "CommitCommentEvent",
        events::ISSUES => "IssuesEvent",
        events::ISSUE_COMMENT => "IssueCommentEvent",
        events::PULL_REQUEST => "PullRequestEvent",
        events::PULL_REQUEST_REVIEW => "PullRequestReviewEvent",
        events::PULL_REQUEST_REVIEW_COMMENT => "PullRequestReviewCommentEvent",
        _ => return Ok(()),
    };
    let public = public && (kind != "ForkEvent" || payload["forkee"]["private"] == false);
    let Some(actor) = payload["sender"]["id"].as_i64() else {
        return Ok(());
    };
    codeyard_db_repos::events::record_event(pool, repo.id, Some(actor), kind, payload, public).await
}

async fn people(pool: &DbPool, mut ids: Vec<i64>) -> People {
    ids.sort_unstable();
    ids.dedup();
    let mut found = HashMap::new();
    for id in ids {
        match codeyard_db_identity::users::accounts::get_user_by_id(pool, id).await {
            Ok(Some(user)) => {
                found.insert(id, Person::of(&user));
            }
            _ => tracing::warn!(user_id = id, "webhook payload user is unavailable"),
        }
    }
    People(found)
}

async fn hooks_for(pool: &DbPool, repo: &Repository) -> anyhow::Result<Vec<Webhook>> {
    let mut hooks =
        codeyard_db_repos::webhooks::list_webhooks(pool, HookOwner::Repository(repo.id)).await?;
    if let Some(org) = repo.owner_org_id {
        hooks.extend(
            codeyard_db_repos::webhooks::list_webhooks(pool, HookOwner::Organization(org)).await?,
        );
    }
    Ok(hooks)
}

use codeyard_webhooks::{
    enqueue_delivery, outgoing, payloads, set_open_issues, BUILD_SLOTS, MAX_PAYLOAD_BYTES, TASKS,
};
