#![recursion_limit = "256"]
use crate::delivery::Outgoing;
use codeyard_database::{models::Webhook, DbPool};
use codeyard_db_repos::webhooks::HookOwner;
use codeyard_types::webhook_events as events;
use serde_json::Value;
use std::{
    collections::HashMap,
    sync::{Arc, LazyLock, Mutex},
    time::{Duration, Instant},
};
use tokio::sync::Semaphore;
use tokio_util::task::TaskTracker;
pub mod delivery;
pub mod deployment_protection;
pub mod github_apps;
pub mod panel;
pub mod payloads;
pub mod settings;

/// Sends that are queued or running. A send that finds the queue full is dropped and logged.
const QUEUE_LIMIT: usize = 256;
/// Body bytes those sends hold. Bodies may be up to `MAX_PAYLOAD_BYTES` each, so the count
/// alone would let the queue hold half a gigabyte.
const QUEUE_BYTES: usize = 64 * 1024 * 1024;
/// Sends running at once.
const SENDS_AT_ONCE: usize = 4;

/// Includes a maximum-size Unicode comment, JSON escaping and form-encoding expansion.
pub const MAX_PAYLOAD_BYTES: usize = 2 * 1024 * 1024;
pub static TASKS: LazyLock<TaskTracker> = LazyLock::new(TaskTracker::new);
pub static BUILD_SLOTS: LazyLock<Arc<Semaphore>> = LazyLock::new(|| Arc::new(Semaphore::new(4)));

/// Stops new webhook sends and waits for the running ones until `deadline`.
pub async fn shutdown(deadline: tokio::time::Instant) {
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
const OWNER_QUEUE_LIMIT: usize = codeyard_db_repos::webhooks::MAX_HOOKS_PER_OWNER;
const HOOK_QUEUE_LIMIT: usize = 16;
const MANUAL_PER_MINUTE: usize = 10;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Owner {
    Native(HookOwner),
    App(i64),
}
impl From<HookOwner> for Owner {
    fn from(owner: HookOwner) -> Self {
        Self::Native(owner)
    }
}

#[derive(Default)]
struct Queue {
    total: usize,
    bytes: usize,
    owners: HashMap<(usize, Owner), OwnerQueue>,
}

struct OwnerQueue {
    // Keep the pool identity alive while a manual-send rate window remains.
    _pool: DbPool,
    pending: usize,
    hooks: HashMap<Option<i64>, HookQueue>,
    manual: Vec<Instant>,
}

struct HookQueue {
    pending: usize,
    slot: Arc<Semaphore>,
}

impl Queue {
    fn take(
        &mut self,
        pool: &DbPool,
        owner: impl Into<Owner>,
        hook: Option<i64>,
        manual: bool,
        bytes: usize,
    ) -> Option<Arc<Semaphore>> {
        let owner = owner.into();
        let now = Instant::now();
        self.owners.retain(|_, queue| {
            queue
                .manual
                .retain(|at| now.duration_since(*at) < Duration::from_secs(60));
            queue.pending > 0 || !queue.manual.is_empty()
        });
        if self.total >= QUEUE_LIMIT || self.bytes.saturating_add(bytes) > QUEUE_BYTES {
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
            });
        if queue.pending >= OWNER_QUEUE_LIMIT
            || queue
                .hooks
                .get(&hook)
                .is_some_and(|hook| hook.pending >= HOOK_QUEUE_LIMIT)
            || (manual && queue.manual.len() >= MANUAL_PER_MINUTE)
        {
            return None;
        }
        queue.pending += 1;
        let hook = queue.hooks.entry(hook).or_insert_with(|| HookQueue {
            pending: 0,
            slot: Arc::new(Semaphore::new(1)),
        });
        hook.pending += 1;
        if manual {
            queue.manual.push(now);
        }
        self.total += 1;
        self.bytes += bytes;
        Some(hook.slot.clone())
    }

    fn release(&mut self, owner: (usize, impl Into<Owner>), hook: Option<i64>, bytes: usize) {
        let owner = (owner.0, owner.1.into());
        let queue = self.owners.get_mut(&owner).expect("queued owner");
        queue.pending -= 1;
        let held = queue.hooks.get_mut(&hook).expect("queued hook");
        held.pending -= 1;
        if held.pending == 0 {
            queue.hooks.remove(&hook);
        }
        self.total -= 1;
        self.bytes -= bytes;
        if queue.pending == 0 && queue.manual.is_empty() {
            self.owners.remove(&owner);
        }
    }
}

/// A place in the queue, freed when dropped. Each hook runs only one delivery at once.
struct Ticket {
    owner: (usize, Owner),
    hook: Option<i64>,
    bytes: usize,
    slot: Arc<Semaphore>,
}

impl Ticket {
    fn take(
        pool: &DbPool,
        owner: impl Into<Owner>,
        hook: Option<i64>,
        manual: bool,
        bytes: usize,
    ) -> Option<Self> {
        let owner = owner.into();
        let slot = QUEUED
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take(pool, owner, hook, manual, bytes)?;
        Some(Self {
            owner: (pool_id(pool), owner),
            hook,
            bytes,
            slot,
        })
    }
}

impl Drop for Ticket {
    fn drop(&mut self) {
        QUEUED
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .release(self.owner, self.hook, self.bytes);
    }
}

fn pool_id(pool: &DbPool) -> usize {
    match pool {
        DbPool::Postgres(pool) => Arc::as_ptr(&pool.connect_options()) as usize,
        DbPool::Sqlite(pool) => Arc::as_ptr(&pool.connect_options()) as usize,
    }
}

fn hook_owner(hook: &Webhook) -> Option<Owner> {
    if let Some(id) = hook.app_id {
        return Some(Owner::App(id));
    }
    match (hook.repo_id, hook.org_id) {
        (Some(id), None) => Some(HookOwner::Repository(id).into()),
        (None, Some(id)) => Some(HookOwner::Organization(id).into()),
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

pub fn enqueue_delivery(pool: &DbPool, hook: Webhook, outgoing: Outgoing) -> bool {
    if outgoing.body.len() > MAX_PAYLOAD_BYTES {
        return false;
    }
    let Some(owner) = hook_owner(&hook) else {
        return false;
    };
    let Some(ticket) = Ticket::take(
        pool,
        owner,
        Some(hook.id),
        !outgoing.automatic,
        outgoing.body.len(),
    ) else {
        return false;
    };
    let pool = pool.clone();
    TASKS.spawn(async move {
        let _ticket = ticket;
        let Ok(_slot) = _ticket.slot.acquire().await else {
            return;
        };
        crate::delivery::send(&pool, &hook, &outgoing).await;
    });
    true
}

/// The send for one hook: a new delivery id, and the body encoded for the hook's content type.
pub fn outgoing(event: &str, action: Option<&str>, hook: &Webhook, json: &str) -> Outgoing {
    Outgoing {
        redelivery: false,
        event: event.to_owned(),
        action: action.map(str::to_owned),
        guid: uuid::Uuid::new_v4().to_string(),
        content_type: hook.content_type.clone(),
        body: encode_body(&hook.content_type, json),
        automatic: event != events::PING,
        manual_actor: None,
    }
}

/// The repository's `open_issues_count` is counted when the payload is made, because the
/// stored column is not kept up to date. Like GitHub's, it counts open pull requests too.
pub async fn set_open_issues(pool: &DbPool, repo_id: i64, payload: &mut Value) {
    let Some(repository) = payload.get_mut("repository").and_then(Value::as_object_mut) else {
        return;
    };
    if let Ok(features) =
        codeyard_db_repos::repositories::records::repository_features(pool, repo_id).await
    {
        crate::payloads::set_features(repository, &features);
    }
    if let Ok(enabled) = codeyard_db_repos::repository_discussions::enabled(pool, repo_id).await {
        repository.insert("has_discussions".into(), enabled.into());
    }
    match codeyard_db_repos::repository_stats::repository_document_facts(pool, repo_id).await {
        Ok(facts) => {
            repository.insert("open_issues_count".into(), facts.open_items.into());
            repository.insert("open_issues".into(), facts.open_items.into());
            repository.insert("language".into(), serde_json::json!(facts.language));
            repository.insert("pushed_at".into(), serde_json::json!(facts.pushed_at));
        }
        Err(error) => tracing::warn!(repo_id, %error, "could not count open issues for a webhook"),
    }
}

/// The body for a content type: the JSON itself, or GitHub's `payload=` form field.
pub fn encode_body(content_type: &str, json: &str) -> String {
    if content_type == crate::settings::FORM_CONTENT_TYPE {
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
        let pool = DbPool::Sqlite(
            codeyard_database::pool::SqlitePool::connect("sqlite::memory:")
                .await
                .unwrap(),
        );
        let mut queue = Queue::default();
        let owner = HookOwner::Repository(1);
        for _ in 0..OWNER_QUEUE_LIMIT {
            assert!(queue.take(&pool, owner, Some(1), false, 0).is_some());
        }
        assert!(queue.take(&pool, owner, Some(2), false, 0).is_none());
        let other = HookOwner::Repository(2);
        let slot = queue.take(&pool, other, Some(3), false, 0).unwrap();
        assert_eq!(slot.available_permits(), 1);
        assert!(
            queue.owners[&(pool_id(&pool), owner.into())].hooks[&Some(1)]
                .slot
                .try_acquire()
                .is_ok()
        );
        assert!(slot.try_acquire().is_ok());
        queue.release((pool_id(&pool), other), Some(3), 0);
        for _ in 0..MANUAL_PER_MINUTE {
            assert!(queue.take(&pool, other, Some(3), true, 0).is_some());
            queue.release((pool_id(&pool), other), Some(3), 0);
        }
        assert!(queue.take(&pool, other, Some(3), true, 0).is_none());
        assert!(queue.take(&pool, other, Some(3), false, 0).is_some());
        let other_pool = DbPool::Sqlite(
            codeyard_database::pool::SqlitePool::connect("sqlite::memory:")
                .await
                .unwrap(),
        );
        assert!(queue.take(&other_pool, other, Some(3), true, 0).is_some());
    }

    #[tokio::test]
    async fn construction_and_delivery_share_the_queue_limit() {
        let pool = DbPool::Sqlite(
            codeyard_database::pool::SqlitePool::connect("sqlite::memory:")
                .await
                .unwrap(),
        );
        let mut queue = Queue::default();
        for id in 0..QUEUE_LIMIT as i64 {
            assert!(queue
                .take(&pool, HookOwner::Repository(id), None, false, 0)
                .is_some());
        }
        assert!(queue
            .take(&pool, HookOwner::Repository(1000), Some(1), false, 0)
            .is_none());
        queue.release((pool_id(&pool), HookOwner::Repository(0)), None, 0);
        assert!(queue
            .take(&pool, HookOwner::Repository(1000), Some(1), false, 0)
            .is_some());
    }

    #[tokio::test]
    async fn queued_bodies_share_one_byte_budget() {
        let pool = DbPool::Sqlite(
            codeyard_database::pool::SqlitePool::connect("sqlite::memory:")
                .await
                .unwrap(),
        );
        let mut queue = Queue::default();
        let half = QUEUE_BYTES / 2;
        assert!(queue
            .take(&pool, HookOwner::Repository(1), Some(1), false, half)
            .is_some());
        assert!(queue
            .take(&pool, HookOwner::Repository(2), Some(2), false, half)
            .is_some());
        assert!(queue
            .take(&pool, HookOwner::Repository(3), Some(3), false, 1)
            .is_none());
        queue.release((pool_id(&pool), HookOwner::Repository(1)), Some(1), half);
        assert!(queue
            .take(&pool, HookOwner::Repository(3), Some(3), false, half)
            .is_some());
        assert_eq!(queue.bytes, QUEUE_BYTES);
    }
}

pub mod comment_documents;
pub mod issue_documents;
