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
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;
use tokio_util::task::TaskTracker;

/// Sends that are queued or running. A send that finds the queue full is dropped and logged.
const QUEUE_LIMIT: usize = 256;
/// Sends running at once.
const SENDS_AT_ONCE: usize = 4;

/// Maximum encoded body retained per queued delivery (at most 64 MiB for the queue).
pub const MAX_PAYLOAD_BYTES: usize = 256 * 1024;
static TASKS: LazyLock<TaskTracker> = LazyLock::new(TaskTracker::new);
static BUILD_SLOTS: LazyLock<Semaphore> = LazyLock::new(|| Semaphore::new(4));

pub(crate) async fn shutdown(deadline: tokio::time::Instant) {
    TASKS.close();
    if tokio::time::timeout_at(deadline, TASKS.wait())
        .await
        .is_err()
    {
        tracing::warn!("webhook shutdown grace expired");
    }
}

static QUEUED: LazyLock<Mutex<Queue>> = LazyLock::new(|| Mutex::new(Queue::default()));
static SLOTS: LazyLock<Semaphore> = LazyLock::new(|| Semaphore::new(SENDS_AT_ONCE));
const OWNER_QUEUE_LIMIT: usize = 16;
const HOOK_QUEUE_LIMIT: usize = 16;
const MANUAL_PER_MINUTE: usize = 10;

#[derive(Default)]
struct Queue {
    total: usize,
    owners: HashMap<(usize, HookOwner), OwnerQueue>,
}

struct OwnerQueue {
    // Keep the pool identity alive while a manual-send rate window remains.
    _pool: DbPool,
    pending: usize,
    hooks: HashMap<i64, usize>,
    manual: Vec<Instant>,
    slot: Arc<Semaphore>,
}

impl Queue {
    fn take(
        &mut self,
        pool: &DbPool,
        owner: HookOwner,
        hook: Option<i64>,
        manual: bool,
    ) -> Option<Arc<Semaphore>> {
        let now = Instant::now();
        self.owners.retain(|_, queue| {
            queue
                .manual
                .retain(|at| now.duration_since(*at) < Duration::from_secs(60));
            queue.pending > 0 || !queue.manual.is_empty()
        });
        if self.total >= QUEUE_LIMIT {
            return None;
        }
        let queue = self
            .owners
            .entry((pool_id(pool), owner))
            .or_insert_with(|| OwnerQueue {
                _pool: pool.clone(),
                pending: 0,
                hooks: HashMap::new(),
                manual: Vec::new(),
                slot: Arc::new(Semaphore::new(1)),
            });
        if queue.pending >= OWNER_QUEUE_LIMIT
            || hook.is_some_and(|id| queue.hooks.get(&id).copied().unwrap_or(0) >= HOOK_QUEUE_LIMIT)
            || (manual && queue.manual.len() >= MANUAL_PER_MINUTE)
        {
            return None;
        }
        queue.pending += 1;
        if let Some(id) = hook {
            *queue.hooks.entry(id).or_default() += 1;
        }
        if manual {
            queue.manual.push(now);
        }
        self.total += 1;
        Some(queue.slot.clone())
    }

    fn release(&mut self, owner: (usize, HookOwner), hook: Option<i64>) {
        let queue = self.owners.get_mut(&owner).expect("queued owner");
        queue.pending -= 1;
        if let Some(id) = hook {
            let count = queue.hooks.get_mut(&id).expect("queued hook");
            *count -= 1;
            if *count == 0 {
                queue.hooks.remove(&id);
            }
        }
        self.total -= 1;
        if queue.pending == 0 && queue.manual.is_empty() {
            self.owners.remove(&owner);
        }
    }
}

/// A place in the queue, freed when dropped. Each owner runs only one delivery at once.
struct Ticket {
    owner: (usize, HookOwner),
    hook: Option<i64>,
    slot: Arc<Semaphore>,
}

impl Ticket {
    fn take(pool: &DbPool, owner: HookOwner, hook: Option<i64>, manual: bool) -> Option<Self> {
        let slot = QUEUED.lock().unwrap().take(pool, owner, hook, manual)?;
        Some(Self {
            owner: (pool_id(pool), owner),
            hook,
            slot,
        })
    }
}

impl Drop for Ticket {
    fn drop(&mut self) {
        QUEUED.lock().unwrap().release(self.owner, self.hook);
    }
}

fn pool_id(pool: &DbPool) -> usize {
    match pool {
        DbPool::Postgres(pool) => Arc::as_ptr(&pool.connect_options()) as usize,
        DbPool::Sqlite(pool) => Arc::as_ptr(&pool.connect_options()) as usize,
    }
}

fn hook_owner(hook: &Webhook) -> Option<HookOwner> {
    match (hook.repo_id, hook.org_id) {
        (Some(id), None) => Some(HookOwner::Repository(id)),
        (None, Some(id)) => Some(HookOwner::Organization(id)),
        _ => None,
    }
}

/// Queues one send to `hook`. Returns false when the queue is full.
pub fn enqueue(pool: &DbPool, hook: Webhook, outgoing: Outgoing) -> bool {
    if TASKS.is_closed() {
        return false;
    }
    enqueue_delivery(pool, hook, outgoing)
}

fn enqueue_delivery(pool: &DbPool, hook: Webhook, outgoing: Outgoing) -> bool {
    if outgoing.body.len() > MAX_PAYLOAD_BYTES {
        return false;
    }
    let Some(owner) = hook_owner(&hook) else {
        return false;
    };
    let Some(ticket) = Ticket::take(pool, owner, Some(hook.id), !outgoing.automatic) else {
        return false;
    };
    let pool = pool.clone();
    TASKS.spawn(async move {
        let _ticket = ticket;
        let Ok(_slot) = _ticket.slot.acquire().await else {
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
        automatic: event != events::PING,
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
    if TASKS.is_closed() {
        return;
    }
    let Some(ticket) = Ticket::take(pool, HookOwner::Repository(repo.id), None, false) else {
        tracing::warn!(event, "webhook queue is full; an event was dropped");
        return;
    };
    let pool = pool.clone();
    let repo = repo.clone();
    TASKS.spawn(async move {
        let ticket = ticket;
        let Ok(_build_slot) = BUILD_SLOTS.acquire().await else {
            return;
        };
        // Never select a former organization's hooks using a pre-transfer snapshot.
        let Ok(Some(current)) =
            db::repositories::records::get_repository_by_id(&pool, repo.id).await
        else {
            return;
        };
        if current.owner_user_id != repo.owner_user_id || current.owner_org_id != repo.owner_org_id
        {
            return;
        }
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
        let payload = match TASKS.spawn_blocking(move || build(&owner, &people)).await {
            Ok(payload) => payload,
            Err(error) => {
                tracing::warn!(repo_id = repo.id, %error, "could not build a webhook payload");
                return;
            }
        };
        let json = payload.to_string();
        if json.len() > MAX_PAYLOAD_BYTES {
            tracing::warn!(event, "webhook payload is too large; an event was dropped");
            return;
        }
        let action = payload
            .get("action")
            .and_then(Value::as_str)
            .map(str::to_owned);
        // The deliveries take their own places, so the repository's last hook is not crowded out.
        drop(ticket);
        for hook in hooks {
            let outgoing = outgoing(event, action.as_deref(), &hook, &json);
            if !enqueue_delivery(&pool, hook, outgoing) {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shutdown_drains_registered_webhook_work() {
        const CHILD: &str = "CODEYARD_WEBHOOK_SHUTDOWN_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .arg("shutdown_drains_registered_webhook_work")
                .env(CHILD, "1")
                .status()
                .unwrap();
            assert!(status.success());
            return;
        }
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let (done, finished) = tokio::sync::oneshot::channel();
                TASKS.spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                    done.send(()).unwrap();
                });
                shutdown(tokio::time::Instant::now() + std::time::Duration::from_secs(1)).await;
                assert!(finished.await.is_ok());
                assert!(TASKS.is_closed() && TASKS.is_empty());
            });
    }

    #[tokio::test]
    async fn owners_have_separate_capacity_and_manual_rate_limits() {
        let pool = DbPool::Sqlite(sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap());
        let mut queue = Queue::default();
        let owner = HookOwner::Repository(1);
        for _ in 0..OWNER_QUEUE_LIMIT {
            assert!(queue.take(&pool, owner, Some(1), false).is_some());
        }
        assert!(queue.take(&pool, owner, Some(2), false).is_none());
        let other = HookOwner::Repository(2);
        let slot = queue.take(&pool, other, Some(3), false).unwrap();
        assert_eq!(slot.available_permits(), 1);
        assert!(queue.owners[&(pool_id(&pool), owner)]
            .slot
            .try_acquire()
            .is_ok());
        assert!(slot.try_acquire().is_ok());
        queue.release((pool_id(&pool), other), Some(3));
        for _ in 0..MANUAL_PER_MINUTE {
            assert!(queue.take(&pool, other, Some(3), true).is_some());
            queue.release((pool_id(&pool), other), Some(3));
        }
        assert!(queue.take(&pool, other, Some(3), true).is_none());
        assert!(queue.take(&pool, other, Some(3), false).is_some());
        let other_pool =
            DbPool::Sqlite(sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap());
        assert!(queue.take(&other_pool, other, Some(3), true).is_some());
    }

    #[tokio::test]
    async fn construction_and_delivery_share_the_queue_limit() {
        let pool = DbPool::Sqlite(sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap());
        let mut queue = Queue::default();
        for id in 0..QUEUE_LIMIT as i64 {
            assert!(queue
                .take(&pool, HookOwner::Repository(id), None, false)
                .is_some());
        }
        assert!(queue
            .take(&pool, HookOwner::Repository(1000), Some(1), false)
            .is_none());
        queue.release((pool_id(&pool), HookOwner::Repository(0)), None);
        assert!(queue
            .take(&pool, HookOwner::Repository(1000), Some(1), false)
            .is_some());
    }
}
