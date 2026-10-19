//! Webhooks: subscriptions on repositories and organizations, and the background sends that
//! follow a committed write. Callers never wait for a send, and a send never fails a request.
pub mod delivery;
pub mod events;
pub mod outbound;
pub mod panel;
pub mod payloads;
pub mod ref_changes;
pub mod settings;

use crate::backend::db::models::{Repository, Webhook};
use crate::backend::db::webhooks::HookOwner;
use crate::backend::db::{self, DbPool};
use delivery::Outgoing;
use payloads::Person;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::LazyLock;
use tokio::sync::Semaphore;

/// Sends that are queued or running. A send that finds the queue full is dropped and logged.
const QUEUE_LIMIT: usize = 256;
/// Sends running at once.
const SENDS_AT_ONCE: usize = 4;

static QUEUED: AtomicUsize = AtomicUsize::new(0);
static SLOTS: LazyLock<Semaphore> = LazyLock::new(|| Semaphore::new(SENDS_AT_ONCE));

/// A place in the queue, freed when dropped.
struct Ticket;

impl Ticket {
    fn take() -> Option<Self> {
        if QUEUED.fetch_add(1, Ordering::AcqRel) >= QUEUE_LIMIT {
            QUEUED.fetch_sub(1, Ordering::AcqRel);
            return None;
        }
        Some(Self)
    }
}

impl Drop for Ticket {
    fn drop(&mut self) {
        QUEUED.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Queues one send to `hook`. Returns false when the queue is full.
pub fn enqueue(pool: &DbPool, hook: Webhook, outgoing: Outgoing) -> bool {
    let Some(ticket) = Ticket::take() else {
        return false;
    };
    let pool = pool.clone();
    tokio::spawn(async move {
        let _ticket = ticket;
        let Ok(_slot) = SLOTS.acquire().await else {
            return;
        };
        delivery::send(&pool, &hook, &outgoing).await;
    });
    true
}

/// The send for one hook: a new delivery id, and the body encoded for the hook's content type.
pub fn outgoing(event: &str, action: Option<&str>, hook: &Webhook, json: &str) -> Outgoing {
    Outgoing {
        event: event.to_owned(),
        action: action.map(str::to_owned),
        guid: uuid::Uuid::new_v4().to_string(),
        content_type: hook.content_type.clone(),
        body: encode_body(&hook.content_type, json),
    }
}

/// The users a payload names, looked up only when some hook wants the event.
#[derive(Debug, Default)]
pub struct People(HashMap<i64, Person>);

impl People {
    /// A user that is missing shows an empty login instead of failing the payload.
    pub fn get(&self, id: i64) -> Person {
        self.0.get(&id).cloned().unwrap_or(Person {
            login: String::new(),
            id,
        })
    }
}

/// Sends `event` to every active hook of the repository, and of its organization, that
/// subscribes to it. `user_ids` are the users the payload names, and `build` gets the owner's
/// name and those users, and makes the payload. Both run in the background, and `build` runs on a
/// blocking thread only when some hook wants the event. A send never fails the caller.
pub fn emit<F>(pool: &DbPool, repo: &Repository, event: &'static str, user_ids: Vec<i64>, build: F)
where
    F: FnOnce(&str, &People) -> Value + Send + 'static,
{
    let pool = pool.clone();
    let repo = repo.clone();
    tokio::spawn(async move {
        let hooks = match hooks_for(&pool, &repo).await {
            Ok(hooks) => hooks,
            Err(error) => {
                tracing::warn!(repo_id = repo.id, %error, "could not load webhooks");
                return;
            }
        };
        let hooks: Vec<Webhook> = hooks
            .into_iter()
            .filter(|hook| hook.active && events::subscribes(&hook.events, event))
            .collect();
        if hooks.is_empty() {
            return;
        }
        let owner = db::repositories::records::repository_owner_names(&pool, &[repo.id])
            .await
            .ok()
            .and_then(|mut names| names.remove(&repo.id))
            .unwrap_or_default();
        let people = people(&pool, user_ids).await;
        let payload = match tokio::task::spawn_blocking(move || build(&owner, &people)).await {
            Ok(payload) => payload,
            Err(error) => {
                tracing::warn!(repo_id = repo.id, %error, "could not build a webhook payload");
                return;
            }
        };
        let json = payload.to_string();
        let action = payload
            .get("action")
            .and_then(Value::as_str)
            .map(str::to_owned);
        for hook in hooks {
            let outgoing = outgoing(event, action.as_deref(), &hook, &json);
            if !enqueue(&pool, hook, outgoing) {
                tracing::warn!(event, "webhook queue is full; a delivery was dropped");
            }
        }
    });
}

async fn people(pool: &DbPool, mut ids: Vec<i64>) -> People {
    ids.sort_unstable();
    ids.dedup();
    let mut found = HashMap::new();
    for id in ids {
        match db::users::accounts::get_user_by_id(pool, id).await {
            Ok(Some(user)) => {
                found.insert(id, Person::of(&user));
            }
            _ => tracing::warn!(user_id = id, "webhook payload user is unavailable"),
        }
    }
    People(found)
}

async fn hooks_for(pool: &DbPool, repo: &Repository) -> anyhow::Result<Vec<Webhook>> {
    let mut hooks = db::webhooks::list_webhooks(pool, HookOwner::Repository(repo.id)).await?;
    if let Some(org) = repo.owner_org_id {
        hooks.extend(db::webhooks::list_webhooks(pool, HookOwner::Organization(org)).await?);
    }
    Ok(hooks)
}

/// The body for a content type: the JSON itself, or GitHub's `payload=` form field.
pub fn encode_body(content_type: &str, json: &str) -> String {
    if content_type == settings::FORM_CONTENT_TYPE {
        let encoded: String = url::form_urlencoded::byte_serialize(json.as_bytes()).collect();
        format!("payload={encoded}")
    } else {
        json.to_owned()
    }
}
