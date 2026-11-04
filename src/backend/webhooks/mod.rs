//! Webhooks: subscriptions on repositories and organizations, and the background sends that
//! follow a committed write. Callers never wait for a send, and a send never fails a request.
pub mod delivery;
pub mod deployment_protection;
pub mod documents;

pub mod github_apps;
pub mod outbound;
pub mod panel;
pub mod payloads;
pub mod ref_changes;
pub mod settings;
pub mod statuses;

use codeyard_db_core::models::Repository;
use codeyard_db_core::models::Webhook;
use codeyard_db_core::DbPool;
use codeyard_db_repos::webhooks::HookOwner;
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
/// Body bytes those sends hold. Bodies may be up to `MAX_PAYLOAD_BYTES` each, so the count
/// alone would let the queue hold half a gigabyte.
const QUEUE_BYTES: usize = 64 * 1024 * 1024;
/// Sends running at once.
const SENDS_AT_ONCE: usize = 4;

/// Includes a maximum-size Unicode comment, JSON escaping and form-encoding expansion.
pub const MAX_PAYLOAD_BYTES: usize = 2 * 1024 * 1024;
static TASKS: LazyLock<TaskTracker> = LazyLock::new(TaskTracker::new);
static BUILD_SLOTS: LazyLock<Arc<Semaphore>> = LazyLock::new(|| Arc::new(Semaphore::new(4)));

/// Stops new webhook sends and waits for the running ones until `deadline`.
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

fn enqueue_delivery(pool: &DbPool, hook: Webhook, outgoing: Outgoing) -> bool {
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
        delivery::send(&pool, &hook, &outgoing).await;
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

pub(crate) async fn emit_repository_dispatch(
    pool: &DbPool,
    repo: &Repository,
    actor: i64,
    sender: i64,
    event_type: String,
    client_payload: Value,
    credential: codeyard_db_core::queries::ci::CiCredential,
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
    dispatch_actor: Option<(i64, codeyard_db_core::queries::ci::CiCredential)>,
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
            match github_apps::has_subscription(pool, repo.id, event).await {
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
            Ok(Some(org)) => payload["organization"] = payloads::organization(&org),
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(repo_id=repo.id, %error, "could not load webhook organization")
            }
        }
    }
    set_open_issues(pool, repo.id, &mut payload).await;
    documents::complete(pool, &current, &owner, &mut payload).await;
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
        github_apps::emit_repository(pool, repo, event, action, &guid, &payload).await
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

/// The repository's `open_issues_count` is counted when the payload is made, because the
/// stored column is not kept up to date. Like GitHub's, it counts open pull requests too.
pub(crate) async fn set_open_issues(pool: &DbPool, repo_id: i64, payload: &mut Value) {
    let Some(repository) = payload.get_mut("repository").and_then(Value::as_object_mut) else {
        return;
    };
    if let Ok(features) =
        codeyard_db_repos::repositories::records::repository_features(pool, repo_id).await
    {
        payloads::set_features(repository, &features);
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
        let other_pool =
            DbPool::Sqlite(sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap());
        assert!(queue.take(&other_pool, other, Some(3), true, 0).is_some());
    }

    #[tokio::test]
    async fn construction_and_delivery_share_the_queue_limit() {
        let pool = DbPool::Sqlite(sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap());
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
        let pool = DbPool::Sqlite(sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap());
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
