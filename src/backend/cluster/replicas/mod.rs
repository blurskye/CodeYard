//! Replica copies. Each storage node makes the copies its own repositories need, as the
//! replica setting says, brings the copies it holds up to date, and removes copies it no
//! longer needs. It also takes over the fork families of a primary that has gone offline,
//! when it holds a copy with every write of each member. A node acts only on its own
//! primaries and its own copies, so the nodes coordinate through the database alone.
mod plan;
mod promote;
mod transfer;

pub use promote::{promote, repository_name, take_over, Promotion, Silent, TAKE_OVER_AFTER};

use super::{rpc, NodeContext};
use crate::backend::db::{
    self,
    models::{ClusterNode, HeldReplica, ReplicaSetting},
    DbPool,
};
use crate::backend::git::maintenance;
use crate::backend::repositories::store::RepoError;
use crate::backend::storage::local::{remove_stale_temporaries, STALE_AFTER};
use anyhow::{Context, Result};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::{Path, PathBuf},
    str::FromStr,
    time::{Duration, Instant},
};
use tokio::{sync::watch, task::JoinSet};
use tracing::{debug, info, warn};
use uuid::Uuid;

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn busy_copies_wait_for_the_next_pass_without_increasing_failure_backoff() {
        let mut copying = Copying::default();
        let mut retry = Retry::default();
        let task = copying
            .running
            .spawn(async { (42, Err(RepoError::replica_busy().into())) });
        copying.directories.insert(task.id(), Uuid::new_v4());
        copying.finish(&mut retry).await;
        assert!(retry.waiting(42));
        assert_eq!(retry.failures[&42].0, 0);
        assert!(copying.directories.is_empty());
        retry.failed(42);
        assert_eq!(retry.failures[&42].0, 1);
    }

    #[tokio::test]
    async fn completed_slots_refill_from_the_pending_queue_without_a_database_scan() {
        let key = crate::backend::cluster::NodeKey::generate();
        let context = NodeContext {
            pool: None,
            iroh: crate::backend::cluster::IrohDialer::new(
                crate::backend::cluster::Relay::Off,
                key.clone(),
            ),
            key,
            node_id: 1,
            repos_path: PathBuf::new(),
            peers: Default::default(),
            unreachable: Default::default(),
        };
        let held: Vec<_> = (1..=5)
            .map(|id| HeldReplica {
                repository_id: id,
                disk_path: format!("/repos/{}.git", Uuid::new_v4()),
                primary_node_id: Some(2),
                applied_version: -1,
                write_version: 0,
            })
            .collect();
        let mut copying = Copying::default();
        let mut retry = Retry::default();
        copying.start(&context, &held, &retry);
        assert_eq!(copying.pending.len(), 3);
        copying.finish(&mut retry).await;
        copying.start_pending(&context, &retry);
        assert_eq!(copying.pending.len(), 1);
        copying.finish(&mut retry).await;
        copying.start_pending(&context, &retry);
        copying.finish(&mut retry).await;
        assert!(copying.pending.is_empty());
        assert_eq!(retry.failures.len(), 5);
    }

    #[test]
    fn surplus_names_include_only_real_copies_and_numeric_quarantines() {
        let uuid = Uuid::new_v4();
        assert_eq!(copy_directory_uuid(&format!("{uuid}.git")), Some(uuid));
        assert_eq!(
            copy_directory_uuid(&format!("{uuid}.git.unusable-123")),
            Some(uuid)
        );
        for suffix in [
            ".git.unusable-",
            ".git.unusable-abc",
            ".git.unusable-123/other",
        ] {
            assert_eq!(copy_directory_uuid(&format!("{uuid}{suffix}")), None);
        }
    }
}

/// How often a storage node runs a pass.
pub const INTERVAL: Duration = Duration::from_secs(5);
/// Copies brought up to date at the same time.
const PARALLEL: usize = 2;
/// A failed copy waits this long before the next attempt. The wait doubles, up to the cap.
/// The cap is also how long a copy stays behind after its primary comes back from an
/// outage, so it is short; a failed attempt against a dead node is one quick request.
const RETRY_FIRST: Duration = Duration::from_secs(5);
const RETRY_CAP: Duration = Duration::from_secs(30);
/// Copies are kept here, under the repositories root, never at a primary's location.
const MIRRORS: &str = "mirrors";

/// Runs passes every `INTERVAL` until `stop` is set. A failed pass is logged and the next
/// one tries again. A copy that finishes frees its slot at once, so the next copy in line
/// starts without waiting for the interval.
pub async fn run(context: NodeContext, mut stop: watch::Receiver<bool>) {
    let mut retry = Retry::default();
    let mut silent = Silent::default();
    // Dropping this on shutdown aborts the copies still running.
    let mut copying = Copying::default();
    let mut ticker = tokio::time::interval(INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        let periodic = tokio::select! {
            _ = stop.changed() => break,
            _ = ticker.tick() => true,
            Some(joined) = copying.running.join_next_with_id(), if !copying.running.is_empty() => {
                copying.finished(joined, &mut retry);
                false
            }
        };
        tokio::select! {
            _ = stop.changed() => break,
            result = async {
                if periodic {
                    step(&context, &mut retry, &mut silent, &mut copying).await
                } else {
                    copying.start_pending(&context, &retry);
                    Ok(())
                }
            } => {
                if let Err(error) = result {
                    warn!(%error, "replica pass failed");
                }
            }
        }
    }
}

/// One pass that also waits for the copies it started, for an operator or a test that wants
/// the node current when it returns. `run` never waits for copies. Pass the same `copying`
/// each time, as `run` does, so each pass carries on from the last copy it started.
pub async fn pass(
    context: &NodeContext,
    retry: &mut Retry,
    silent: &mut Silent,
    copying: &mut Copying,
) -> Result<()> {
    step(context, retry, silent, copying).await?;
    copying.finish(retry).await;
    Ok(())
}

/// One step: take over repositories whose primary is offline, assign copies for this node's
/// repositories, put directories where their role says, start copies that are behind, remove
/// copies no longer needed and pack files a crash left behind. Copies run on after the step
/// returns, so a long one delays nothing else. Only database failures are errors; a copy that
/// cannot be made is logged and retried later.
async fn step(
    context: &NodeContext,
    retry: &mut Retry,
    silent: &mut Silent,
    copying: &mut Copying,
) -> Result<()> {
    copying.reap(retry);
    let pool = context.database()?;
    let node = context.node_id;
    take_over(pool, node, &context.repos_path, silent, TAKE_OVER_AFTER).await?;
    let setting = ReplicaSetting::from_str(&db::cluster::get_replicas(pool).await?)?;
    let primaries = group_primaries(db::cluster::list_primary_repositories(pool, node).await?);
    assign(pool, node, setting, &primaries).await?;
    place_and_start(context, &primaries, retry, copying).await?;
    remove_surplus(context, pool, copying).await;
    remove_stale_packs(context).await;
    Ok(())
}

/// Failed copies, and when each may be tried again.
#[derive(Default)]
pub struct Retry {
    failures: HashMap<i64, (u32, Instant)>,
}

impl Retry {
    fn waiting(&self, repository_id: i64) -> bool {
        self.failures
            .get(&repository_id)
            .is_some_and(|(_, next)| Instant::now() < *next)
    }

    fn failed(&mut self, repository_id: i64) {
        let entry = self
            .failures
            .entry(repository_id)
            .or_insert((0, Instant::now()));
        entry.0 = entry.0.saturating_add(1);
        let delay = RETRY_FIRST
            .checked_mul(2u32.saturating_pow(entry.0 - 1))
            .map_or(RETRY_CAP, |delay| delay.min(RETRY_CAP));
        entry.1 = Instant::now() + delay;
    }

    fn busy(&mut self, repository_id: i64) {
        self.failures
            .insert(repository_id, (0, Instant::now() + INTERVAL));
    }

    fn succeeded(&mut self, repository_id: i64) {
        self.failures.remove(&repository_id);
    }
}

/// A repository whose primary is this node, and the nodes that hold its copies.
struct Primary {
    repository_id: i64,
    disk_path: String,
    copies: Vec<plan::Holder>,
}

/// The database lists one row per copy; rows of one repository are next to each other.
fn group_primaries(rows: Vec<(i64, String, Option<i64>, Option<i64>)>) -> Vec<Primary> {
    let mut primaries: Vec<Primary> = Vec::new();
    for (repository_id, disk_path, node, applied_version) in rows {
        let copy = node.map(|node| plan::Holder {
            node,
            applied: applied_version.is_some_and(|version| version >= 0),
        });
        match primaries.last_mut() {
            Some(last) if last.repository_id == repository_id => last.copies.extend(copy),
            _ => primaries.push(Primary {
                repository_id,
                disk_path,
                copies: copy.into_iter().collect(),
            }),
        }
    }
    primaries
}

/// Makes this node's repositories have the copies the setting asks for, on storage nodes
/// that are active. Rows are only ever written for repositories this node is primary of.
async fn assign(
    pool: &DbPool,
    node: i64,
    setting: ReplicaSetting,
    primaries: &[Primary],
) -> Result<()> {
    if primaries.is_empty() {
        return Ok(());
    }
    let storage: Vec<ClusterNode> = db::cluster::list_nodes(pool)
        .await?
        .into_iter()
        .filter(|candidate| {
            candidate.status == "active" && candidate.roles.iter().any(|role| role == "storage")
        })
        .collect();
    for primary in primaries {
        let change = plan::plan(setting, node, &storage, &primary.copies);
        for copy_node in change.add {
            db::cluster::insert_replica(pool, primary.repository_id, copy_node, node).await?;
        }
        for copy_node in change.remove {
            db::cluster::delete_replica(pool, primary.repository_id, copy_node, node).await?;
        }
    }
    Ok(())
}

/// Puts each directory where its role says, then starts the copies that are behind.
async fn place_and_start(
    context: &NodeContext,
    primaries: &[Primary],
    retry: &Retry,
    copying: &mut Copying,
) -> Result<()> {
    let node = context.node_id;
    let held = db::cluster::list_held_replicas(context.database()?, node).await?;
    locate(&context.repos_path, node, primaries, &held, copying);
    copying.start(context, &held, retry);
    Ok(())
}

pub(crate) fn copy_path(repos: &Path, uuid: Uuid) -> PathBuf {
    repos.join(MIRRORS).join(format!("{uuid}.git"))
}

fn place_path(repos: &Path, uuid: Uuid) -> PathBuf {
    repos.join(format!("{uuid}.git"))
}

/// A recorded version outlives its directory. Clear it for a copy Git cannot open, before
/// this node advertises itself or considers take-over, so the copy loop rebuilds it even
/// without another write. A copy that cannot be read is cleared too, so take-over never
/// picks it, and the copies after it are still checked. Clearing only stops it from being
/// promoted; the copy loop rebuilds it once it can be read.
pub(crate) async fn reconcile(pool: &DbPool, node: i64, repos: &Path) -> Result<()> {
    for row in db::cluster::list_held_replicas(pool, node).await? {
        let Some(primary) = row.primary_node_id.filter(|primary| *primary != node) else {
            continue;
        };
        if row.applied_version < 0 {
            continue;
        }
        let Ok(uuid) = rpc::repository_uuid(&row.disk_path) else {
            continue;
        };
        let usable = match opens(&copy_path(repos, uuid)).await {
            Ok(usable) => usable,
            Err(error) => {
                warn!(repository = row.repository_id, %error, "could not read a replica copy");
                false
            }
        };
        if !usable {
            db::cluster::set_applied_version(pool, row.repository_id, node, primary, -1).await?;
        }
    }
    Ok(())
}

/// Whether Git can open the copy at `mirror`. Reconcile and the copy path share this check, so
/// a copy counts as usable only when Git can open it.
async fn opens(mirror: &Path) -> Result<bool> {
    let mirror = mirror.to_owned();
    tokio::task::spawn_blocking(move || {
        crate::backend::git::repository::opens_as_repository(&mirror)
    })
    .await
    .context("the copy check stopped")?
}

/// Puts each directory where its role says: a repository this node is primary of moves
/// from `mirrors` to the primary location, and a copy whose primary moved away keeps its
/// directory as a copy. A directory moves only into a free place, and nothing is deleted.
fn locate(repos: &Path, node: i64, primaries: &[Primary], held: &[HeldReplica], copying: &Copying) {
    for primary in primaries {
        let Ok(uuid) = rpc::repository_uuid(&primary.disk_path) else {
            continue;
        };
        if copying.is_running(uuid) {
            continue;
        }
        let (copy, place) = (copy_path(repos, uuid), place_path(repos, uuid));
        if copy.exists() && !place.exists() {
            move_directory(&copy, &place);
        }
    }
    for row in held.iter().filter(|row| row.primary_node_id != Some(node)) {
        let Ok(uuid) = rpc::repository_uuid(&row.disk_path) else {
            continue;
        };
        if copying.is_running(uuid) {
            continue;
        }
        let (copy, place) = (copy_path(repos, uuid), place_path(repos, uuid));
        if place.exists() && !copy.exists() {
            move_directory(&place, &copy);
        }
    }
}

fn move_directory(from: &Path, to: &Path) {
    if let Some(parent) = to.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(error) = std::fs::rename(from, to) {
        warn!(path = %from.display(), %error, "could not move a repository directory");
    }
}

/// Copies that are running, at most `PARALLEL`. Each one brings a copy up to its primary. A
/// copy that fails is retried later, and the others carry on.
#[derive(Default)]
pub struct Copying {
    running: JoinSet<(i64, Result<()>)>,
    /// The directory each running copy writes, which nothing else may move or remove.
    directories: HashMap<tokio::task::Id, Uuid>,
    /// The repository whose copy was started last. The next scan starts after it, so a
    /// repository that is behind at every pass cannot take both slots every time. Kept in
    /// memory only.
    last_started: i64,
    /// Refreshed once per pass, not after every completed copy.
    pending: VecDeque<HeldReplica>,
}

impl Copying {
    fn is_running(&self, directory: Uuid) -> bool {
        self.directories
            .values()
            .any(|running| *running == directory)
    }

    /// Records the copies that have finished, without waiting for the others.
    fn reap(&mut self, retry: &mut Retry) {
        while let Some(joined) = self.running.try_join_next_with_id() {
            self.finished(joined, retry);
        }
    }

    /// Waits for every running copy.
    async fn finish(&mut self, retry: &mut Retry) {
        while let Some(joined) = self.running.join_next_with_id().await {
            self.finished(joined, retry);
        }
    }

    fn finished(
        &mut self,
        joined: Result<(tokio::task::Id, (i64, Result<()>)), tokio::task::JoinError>,
        retry: &mut Retry,
    ) {
        match joined {
            Ok((id, (repository_id, result))) => {
                self.directories.remove(&id);
                match result {
                    Ok(()) => retry.succeeded(repository_id),
                    // The primary is writing to the repository. The next pass asks again.
                    Err(error) if busy(&error) => {
                        retry.busy(repository_id);
                        debug!(
                            repository = repository_id,
                            "a replica copy waits for writes in progress"
                        );
                    }
                    Err(error) => {
                        retry.failed(repository_id);
                        warn!(repository = repository_id, %error, "could not copy a repository to this node");
                    }
                }
            }
            Err(error) => {
                self.directories.remove(&error.id());
                warn!(%error, "a replica copy stopped unexpectedly");
            }
        }
    }

    /// Starts copies for the rows that are behind, while there is room for them. The scan
    /// begins after the copy that started last and wraps around to the lowest id.
    fn start(&mut self, context: &NodeContext, held: &[HeldReplica], retry: &Retry) {
        let node = context.node_id;
        // `held` comes from the database in repository id order.
        let split = held.partition_point(|row| row.repository_id <= self.last_started);
        self.pending = held[split..]
            .iter()
            .chain(&held[..split])
            .filter(|row| {
                row.primary_node_id.is_some_and(|primary| primary != node)
                    && row.applied_version < row.write_version
                    && rpc::repository_uuid(&row.disk_path).is_ok_and(|uuid| !self.is_running(uuid))
            })
            .cloned()
            .collect();
        self.start_pending(context, retry);
    }

    fn start_pending(&mut self, context: &NodeContext, retry: &Retry) {
        while self.running.len() < PARALLEL {
            let Some(row) = self.pending.pop_front() else {
                break;
            };
            if retry.waiting(row.repository_id) {
                continue;
            }
            let Ok(uuid) = rpc::repository_uuid(&row.disk_path) else {
                continue;
            };
            if self.is_running(uuid) {
                continue;
            }
            self.last_started = row.repository_id;
            let context = context.clone();
            let handle = self
                .running
                .spawn(async move { (row.repository_id, copy_one(&context, &row, uuid).await) });
            self.directories.insert(handle.id(), uuid);
        }
    }
}

/// Whether a copy was refused only because its primary had a ref change in progress.
fn busy(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<RepoError>()
        .is_some_and(RepoError::is_replica_busy)
}

/// Brings one copy up to date. The version is read before the transfer starts, so the
/// pack lists at least the refs of that version.
async fn copy_one(context: &NodeContext, row: &HeldReplica, uuid: Uuid) -> Result<()> {
    let pool = context.database()?;
    let primary = row
        .primary_node_id
        .context("the repository has no primary")?;
    let Some(version) = db::cluster::get_write_version(pool, row.repository_id).await? else {
        return Ok(());
    };
    if version > row.applied_version {
        let mirror = copy_path(&context.repos_path, uuid);
        transfer::copy(context, primary, uuid, &mirror).await?;
        // Finish packing before publishing a promotable version: a running maintenance
        // command still needs the mirror directory and prevents relocation.
        maintenance::after_copy(row.repository_id, &mirror).await;
        // Refused if the repository was promoted elsewhere during the copy; the next round
        // copies from the new primary.
        db::cluster::set_applied_version(
            pool,
            row.repository_id,
            context.node_id,
            primary,
            version,
        )
        .await?;
    }
    Ok(())
}

/// Removes copies and former-primary directories this node no longer holds. Copies of its
/// own repositories are kept for `locate`, as are running copies and unplaced primaries.
/// If the database cannot answer, nothing is removed.
async fn remove_surplus(context: &NodeContext, pool: &DbPool, copying: &Copying) {
    let keep = match keep_directories(context).await {
        Ok(mut keep) => {
            keep.extend(copying.directories.values().copied());
            keep
        }
        Err(error) => {
            warn!(%error, "could not check replica copies; none removed");
            return;
        }
    };
    for directory in [context.repos_path.join(MIRRORS), context.repos_path.clone()] {
        let Ok(mut entries) = tokio::fs::read_dir(&directory).await else {
            continue;
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            let uuid = entry.file_name().to_str().and_then(copy_directory_uuid);
            let Some(uuid) = uuid else {
                continue;
            };
            if keep.contains(&uuid) {
                continue;
            }
            if directory == context.repos_path {
                // The keep list may predate a promotion that made this node a copy holder.
                match needed_here(pool, context.node_id, uuid).await {
                    Ok(true) => continue,
                    Ok(false) => {}
                    Err(error) => {
                        warn!(%error, "could not check former primary; none removed");
                        return;
                    }
                }
            }
            match tokio::fs::remove_dir_all(entry.path()).await {
                Ok(()) => {
                    info!(repository = %uuid, "removed a replica copy this node no longer holds")
                }
                Err(error) => warn!(repository = %uuid, %error, "could not remove a replica copy"),
            }
        }
    }
}

/// Recognizes a copy or a numeric quarantine name, without accepting unrelated siblings.
fn copy_directory_uuid(name: &str) -> Option<Uuid> {
    let name = if let Some((name, stamp)) = name.split_once(".unusable-") {
        if stamp.is_empty() || !stamp.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        name
    } else {
        name
    };
    Uuid::parse_str(name.strip_suffix(".git")?).ok()
}

/// Whether this node still needs the directory of `uuid`: an unplaced repository, a primary
/// here, or a copy held here. Read the repository before its copies so a concurrent promotion's
/// new primary and former-primary copy row are both visible.
async fn needed_here(pool: &DbPool, node: i64, uuid: Uuid) -> Result<bool> {
    let Some(repo) = db::cluster::get_repository_by_uuid(pool, &uuid.to_string()).await? else {
        return Ok(false);
    };
    // Rows without placement belong to pre-cluster installations and must survive.
    if repo.storage_node_id.is_none() || repo.storage_node_id == Some(node) {
        return Ok(true);
    }
    let copies = db::cluster::list_replicas(pool, repo.id).await?;
    Ok(copies.iter().any(|(copy, _)| *copy == node))
}

/// Removes pack files that a crash left in the copies directory. A pack that a running copy
/// is writing has a fresh modification time, so it is kept.
async fn remove_stale_packs(context: &NodeContext) {
    let copies = context.repos_path.join(MIRRORS);
    match remove_stale_temporaries(&copies, STALE_AFTER).await {
        Ok(0) => {}
        Ok(removed) => info!(removed, "removed stale replica packs"),
        Err(error) => warn!(%error, "could not remove stale replica packs"),
    }
}

async fn keep_directories(context: &NodeContext) -> Result<HashSet<Uuid>> {
    let pool = context.database()?;
    let node = context.node_id;
    let mut keep = HashSet::new();
    // Copies first. A promotion to this node removes its copy row and makes it primary in one
    // transaction, so the later primary list shows the repository if the copy list did not.
    for row in db::cluster::list_held_replicas(pool, node).await? {
        keep.insert(rpc::repository_uuid(&row.disk_path)?);
    }
    for (_, disk_path, _, _) in db::cluster::list_primary_repositories(pool, node).await? {
        keep.insert(rpc::repository_uuid(&disk_path)?);
    }
    Ok(keep)
}
