//! Replica copies. Each storage node makes the copies its own repositories need, as the
//! replica setting says, brings the copies it holds up to date, and removes copies it no
//! longer needs. It also takes over the fork families of a primary that has gone offline,
//! when it holds a copy with every write of each member. A node acts only on its own
//! primaries and its own copies, so the nodes coordinate through the database alone.
use crate::replicas::promote::take_over;
use crate::replicas::promote::Silent;
use crate::replicas::promote::TAKE_OVER_AFTER;
use anyhow::Context;
use anyhow::Result;
use codeyard_database::models::HeldReplica;
use codeyard_database::models::ReplicaSetting;
use codeyard_database::DbPool;
use codeyard_git::error::RepoError;
use codeyard_git::locks;
use codeyard_git::maintenance;
use codeyard_git::mirror;
use codeyard_git_storage::context::NodeContext;
use codeyard_storage::local::remove_stale_temporaries;
use codeyard_storage::local::STALE_AFTER;
use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::path::Path;
use std::str::FromStr;
use std::time::Duration;
use std::time::Instant;
use tokio::sync::watch;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::debug;
use tracing::info;
use tracing::warn;
use uuid::Uuid;
pub mod promote;
pub mod rebalance;
mod transfer;

/// Operator backup uses the same authenticated pack transfer as replica maintenance.
pub async fn snapshot(
    context: &NodeContext,
    primary: i64,
    repository: Uuid,
    mirror: &Path,
    want: &[String],
) -> Result<()> {
    transfer::snapshot(
        context,
        primary,
        repository,
        mirror,
        want,
        &CancellationToken::new(),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

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
        let key = codeyard_support::node_identity::NodeKey::generate();
        let context = NodeContext {
            pool: None,
            rpc: std::sync::Arc::new(codeyard_repos_rpc::RpcFactory),
            iroh: codeyard_nodes::transport::IrohDialer::new(
                codeyard_nodes::transport::Relay::Off,
                key.clone(),
            ),
            key,
            node_id: 1,
            repos_path: PathBuf::new(),
            peers: Default::default(),
            unreachable: Default::default(),
            clone_cursor: Default::default(),
            ci_replicas: Default::default(),
        };
        let held: Vec<_> = (1..=5)
            .map(|id| HeldReplica {
                repository_id: id,
                disk_path: format!("/repos/{}.git", Uuid::new_v4()),
                primary_node_id: Some(2),
                applied_version: -1,
                write_version: 0,
                fenced: false,
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

    #[tokio::test]
    async fn pending_replica_assignments_reserve_disk_space_across_repositories() {
        let root = tempfile::tempdir().unwrap();
        let pool = codeyard_database::init_db(&format!(
            "sqlite://{}",
            root.path().join("test.db").display()
        ))
        .await
        .unwrap();
        codeyard_database::run_migrations(&pool).await.unwrap();
        let mut nodes = Vec::new();
        const GIB: i64 = 1024 * 1024 * 1024;
        for (name, free) in [("a", 100 * GIB), ("b", 12 * GIB), ("c", 11 * GIB)] {
            let key = codeyard_support::node_identity::NodeKey::generate();
            let node = codeyard_db_placement::cluster::register_own_node(
                &pool,
                &key.public_hex(),
                name,
                &["storage".into()],
                "test",
            )
            .await
            .unwrap();
            codeyard_db_placement::cluster::record_node_heartbeat(
                &pool,
                &key.public_hex(),
                "test",
                "{}",
                Some(free),
            )
            .await
            .unwrap();
            nodes.push(node.id);
        }
        let owner = codeyard_db_identity::users::accounts::create_user(&pool, "owner", None)
            .await
            .unwrap();
        let DbPool::Sqlite(raw) = &pool else {
            unreachable!()
        };
        for index in 0..20 {
            let repo = codeyard_db_repos::repositories::records::create_repository(
                &pool,
                owner.id,
                &format!("repo-{index}"),
                None,
                false,
                &format!("/repos/{}.git", Uuid::new_v4()),
            )
            .await
            .unwrap();
            sqlx::query("UPDATE repositories SET storage_node_id=?1,size_bytes=?2 WHERE id=?3")
                .bind(nodes[0])
                .bind(GIB)
                .bind(repo.id)
                .execute(raw)
                .await
                .unwrap();
        }
        let primaries = group_primaries(
            codeyard_db_placement::cluster::list_primary_repositories(&pool, nodes[0])
                .await
                .unwrap(),
        );
        assign(&pool, nodes[0], ReplicaSetting::Copies(2), &primaries)
            .await
            .unwrap();
        let allocations = codeyard_db_placement::cluster::pending_replica_bytes(&pool)
            .await
            .unwrap();
        assert_eq!(allocations.iter().map(|row| row.1).sum::<i64>(), 22 * GIB);
        assert!(allocations
            .iter()
            .any(|row| row.0 == nodes[1] && row.1 <= 12 * GIB));
        assert!(allocations
            .iter()
            .any(|row| row.0 == nodes[2] && row.1 <= 11 * GIB));
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
/// Runs passes every `INTERVAL` until `stop` is set. A failed pass is logged and the next
/// one tries again. A copy that finishes frees its slot at once, so the next copy in line
/// starts without waiting for the interval.
pub async fn run(context: NodeContext, mut stop: watch::Receiver<bool>) {
    let mut retry = Retry::default();
    let mut silent = Silent::default();
    // Downloads stop cooperatively; installation retains directory protection until joined.
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
    copying.cancel_all();
    copying.finish(&mut retry).await;
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
    let setting =
        ReplicaSetting::from_str(&codeyard_db_placement::cluster::get_replicas(pool).await?)?;
    let primaries = group_primaries(
        codeyard_db_placement::cluster::list_primary_repositories(pool, node).await?,
    );
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

    pub fn failed(&mut self, repository_id: i64) {
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

    pub fn busy(&mut self, repository_id: i64) {
        self.failures
            .insert(repository_id, (0, Instant::now() + INTERVAL));
    }

    pub fn succeeded(&mut self, repository_id: i64) {
        self.failures.remove(&repository_id);
    }
}

/// A repository whose primary is this node, and the nodes that hold its copies.
struct Primary {
    repository_id: i64,
    disk_path: String,
}

/// The database lists one row per copy; rows of one repository are next to each other.
fn group_primaries(rows: Vec<codeyard_db_placement::cluster::PrimaryReplicaRow>) -> Vec<Primary> {
    let mut primaries: Vec<Primary> = Vec::new();
    for (repository_id, disk_path, _, _, _, _) in rows {
        match primaries.last_mut() {
            Some(last) if last.repository_id == repository_id => {}
            _ => primaries.push(Primary {
                repository_id,
                disk_path,
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
    let mut attempted = HashSet::new();
    for primary in primaries {
        if attempted.insert(primary.repository_id) {
            attempted.extend(
                codeyard_db_placement::cluster::assign_replica_family(
                    pool,
                    primary.repository_id,
                    node,
                    setting,
                )
                .await?,
            );
        }
    }
    Ok(())
}

/// Puts each directory where its role says, then starts the copies that are behind.
async fn place_and_start(
    context: &NodeContext,
    primaries: &[Primary],
    retry: &mut Retry,
    copying: &mut Copying,
) -> Result<()> {
    let node = context.node_id;
    let mut held =
        codeyard_db_placement::cluster::list_held_replicas(context.database()?, node).await?;
    let voters = if codeyard_db_placement::cluster::writes::write_settings(context.database()?)
        .await?
        .write_quorum
        .is_some()
    {
        codeyard_db_placement::cluster::writes::held_voters(context.database()?, node).await?
    } else {
        Vec::new()
    };
    copying.voting = voters.iter().map(|row| row.repository_id).collect();
    held.retain(|row| !copying.voting.contains(&row.repository_id));
    held.extend(voters);
    held.sort_by_key(|row| row.repository_id);
    copying.cancel_obsolete(&held);
    let live: HashSet<_> = held.iter().map(|row| row.repository_id).collect();
    retry
        .failures
        .retain(|repository, _| live.contains(repository));
    let failed = locate(
        context.database()?,
        &context.repos_path,
        node,
        primaries,
        &held,
        copying,
    )
    .await;
    held.retain(|row| !failed.contains(&row.repository_id));
    copying.start(context, &held, retry);
    Ok(())
}

/// A recorded version outlives its directory. Clear it for a copy Git cannot open, before
/// this node advertises itself or considers take-over, so the copy loop rebuilds it even
/// without another write. A copy that cannot be read is cleared too, so take-over never
/// picks it, and the copies after it are still checked. Clearing only stops it from being
/// promoted; the copy loop rebuilds it once it can be read.
pub(crate) async fn reconcile(pool: &DbPool, node: i64, repos: &Path) -> Result<()> {
    for row in codeyard_db_placement::cluster::list_held_replicas(pool, node).await? {
        let Some(primary) = row.primary_node_id.filter(|primary| *primary != node) else {
            continue;
        };
        if row.applied_version < 0 {
            continue;
        }
        let Ok(uuid) = codeyard_git_storage::wire::repository_uuid(&row.disk_path) else {
            continue;
        };
        let usable = match connected(&copy_path(repos, uuid)).await {
            Ok(usable) => usable,
            Err(error) => {
                warn!(repository = row.repository_id, %error, "could not read a replica copy");
                false
            }
        };
        if !usable {
            codeyard_db_placement::cluster::set_applied_version(
                pool,
                row.repository_id,
                node,
                primary,
                -1,
            )
            .await?;
        }
    }
    Ok(())
}

/// Puts each directory where its role says: a repository this node is primary of moves
/// from `mirrors` to the primary location, and a copy whose primary moved away keeps its
/// directory as a copy. A directory moves only into a free place, and nothing is deleted.
async fn locate(
    pool: &DbPool,
    repos: &Path,
    node: i64,
    primaries: &[Primary],
    held: &[HeldReplica],
    copying: &Copying,
) -> HashSet<i64> {
    let mut failed = HashSet::new();
    match codeyard_db_placement::cluster::writes::write_settings(pool).await {
        Ok(settings) if settings.write_quorum.is_some() => return failed,
        Err(error) => {
            warn!(%error, "could not check quorum before relocating storage");
            failed.extend(primaries.iter().map(|row| row.repository_id));
            failed.extend(held.iter().map(|row| row.repository_id));
            return failed;
        }
        Ok(_) => {}
    }
    for primary in primaries {
        let Ok(uuid) = codeyard_git_storage::wire::repository_uuid(&primary.disk_path) else {
            continue;
        };
        if copying.is_running(uuid) {
            continue;
        }
        let (copy, place) = (
            copy_path(repos, uuid),
            codeyard_git_storage::repositories::store::primary_path(repos, uuid),
        );
        if tokio::fs::try_exists(&copy).await.unwrap_or(true)
            && !tokio::fs::try_exists(&place).await.unwrap_or(false)
        {
            if let Err(error) =
                move_directory(pool, primary.repository_id, node, true, &copy, &place).await
            {
                warn!(repository=primary.repository_id, %error, "could not relocate a primary directory");
                failed.insert(primary.repository_id);
            }
        }
    }
    for row in held.iter().filter(|row| row.primary_node_id != Some(node)) {
        let Ok(uuid) = codeyard_git_storage::wire::repository_uuid(&row.disk_path) else {
            continue;
        };
        if copying.is_running(uuid) {
            continue;
        }
        let (copy, place) = (
            copy_path(repos, uuid),
            codeyard_git_storage::repositories::store::primary_path(repos, uuid),
        );
        if tokio::fs::try_exists(&place).await.unwrap_or(true) {
            let moved = if tokio::fs::try_exists(&copy).await.unwrap_or(true) {
                Err(anyhow::anyhow!(
                    "both primary and mirror directories exist; refusing to copy ambiguous storage"
                ))
            } else {
                move_directory(pool, row.repository_id, node, false, &place, &copy).await
            };
            if let Err(error) = moved {
                warn!(repository=row.repository_id, %error, "could not relocate a former primary; copy deferred");
                failed.insert(row.repository_id);
            }
        }
    }
    failed
}

async fn move_directory(
    pool: &DbPool,
    repository: i64,
    node: i64,
    primary: bool,
    from: &Path,
    to: &Path,
) -> Result<()> {
    let (primary_path, mirror_path) = if primary { (to, from) } else { (from, to) };
    let guards = tokio::time::timeout(MOVE_WAIT, moving(primary_path, mirror_path)).await?;
    let row = codeyard_db_repos::repositories::records::get_repository_by_id(pool, repository)
        .await?
        .context("repository was deleted before relocation")?;
    anyhow::ensure!(
        row.storage_node_id.is_some() && (row.storage_node_id == Some(node)) == primary,
        "repository placement changed before relocation"
    );
    if !primary {
        anyhow::ensure!(
            codeyard_db_placement::cluster::replica_applied_version(pool, repository, node)
                .await?
                .is_some(),
            "copy assignment changed before relocation"
        );
    }
    let from = from.to_owned();
    let to = to.to_owned();
    tokio::task::spawn_blocking(move || {
        let _guards = guards;
        anyhow::ensure!(!to.try_exists()?, "relocation target already exists");
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::rename(from, to)?;
        Ok::<_, anyhow::Error>(())
    })
    .await?
}

/// Copies that are running, at most `PARALLEL`. Each one brings a copy up to its primary. A
/// copy that fails is retried later, and the others carry on.
#[derive(Default)]
pub struct Copying {
    running: JoinSet<(i64, Result<()>)>,
    /// The directory each running copy writes, which nothing else may move or remove.
    directories: HashMap<tokio::task::Id, Uuid>,
    /// Source identity and cancellation stay owned until the task is joined.
    transfers: HashMap<tokio::task::Id, (i64, Option<i64>, bool, CancellationToken)>,
    /// Voting copies share the same two transfer slots and shutdown owner.
    voting: HashSet<i64>,
    /// The repository whose copy was started last. The next scan starts after it, so a
    /// repository that is behind at every pass cannot take both slots every time. Kept in
    /// memory only.
    last_started: i64,
    /// Refreshed once per pass, not after every completed copy.
    pending: VecDeque<HeldReplica>,
}

impl Copying {
    fn cancel_all(&self) {
        for (_, _, _, stop) in self.transfers.values() {
            stop.cancel();
        }
    }

    fn cancel_obsolete(&self, held: &[HeldReplica]) {
        for (repository, primary, voting, stop) in self.transfers.values() {
            if self.voting.contains(repository) != *voting
                || !held
                    .iter()
                    .any(|row| row.repository_id == *repository && row.primary_node_id == *primary)
            {
                stop.cancel();
            }
        }
    }

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
    pub async fn finish(&mut self, retry: &mut Retry) {
        while let Some(joined) = self.running.join_next_with_id().await {
            self.finished(joined, retry);
        }
    }

    pub fn finished(
        &mut self,
        joined: Result<(tokio::task::Id, (i64, Result<()>)), tokio::task::JoinError>,
        retry: &mut Retry,
    ) {
        match joined {
            Ok((id, (repository_id, result))) => {
                let cancelled = self
                    .transfers
                    .remove(&id)
                    .is_some_and(|(_, _, _, stop)| stop.is_cancelled());
                self.directories.remove(&id);
                if cancelled {
                    retry.succeeded(repository_id);
                    return;
                }
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
                self.transfers.remove(&error.id());
                self.directories.remove(&error.id());
                warn!(%error, "a replica copy stopped unexpectedly");
            }
        }
    }

    /// Starts copies for the rows that are behind, while there is room for them. The scan
    /// begins after the copy that started last and wraps around to the lowest id.
    pub fn start(&mut self, context: &NodeContext, held: &[HeldReplica], retry: &Retry) {
        let node = context.node_id;
        // `held` comes from the database in repository id order.
        let split = held.partition_point(|row| row.repository_id <= self.last_started);
        self.pending = held[split..]
            .iter()
            .chain(&held[..split])
            .filter(|row| {
                (self.voting.contains(&row.repository_id)
                    || row.primary_node_id.is_some_and(|primary| primary != node))
                    && (row.fenced || row.applied_version < row.write_version)
                    && codeyard_git_storage::wire::repository_uuid(&row.disk_path)
                        .is_ok_and(|uuid| !self.is_running(uuid))
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
            let Ok(uuid) = codeyard_git_storage::wire::repository_uuid(&row.disk_path) else {
                continue;
            };
            if self.is_running(uuid) {
                continue;
            }
            self.last_started = row.repository_id;
            let context = context.clone();
            let stop = CancellationToken::new();
            let task_stop = stop.clone();
            let repository = row.repository_id;
            let primary = row.primary_node_id;
            let voting = self.voting.contains(&repository);
            let handle = self.running.spawn(async move {
                (
                    repository,
                    copy_one(&context, &row, uuid, &task_stop, voting).await,
                )
            });
            self.directories.insert(handle.id(), uuid);
            self.transfers
                .insert(handle.id(), (repository, primary, voting, stop));
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
async fn copy_one(
    context: &NodeContext,
    row: &HeldReplica,
    uuid: Uuid,
    stop: &CancellationToken,
    voting: bool,
) -> Result<()> {
    let pool = context.database()?;
    if voting {
        if stop.is_cancelled() {
            return Ok(());
        }
        let view =
            codeyard_db_placement::cluster::writes::recovery_view(pool, row.repository_id).await?;
        if !view.serving
            && view.voters.iter().any(|voter| {
                voter.node_id == context.node_id && voter.applied_version != view.committed_version
            })
        {
            // Reconstruction may outlast a heartbeat. Try retained local objects in
            // this owned slot before a network refill.
            let _ = codeyard_git_storage::repositories::store::RepoStore::recover_term(
                pool,
                row.repository_id,
                context.node_id,
                &context.repos_path,
            )
            .await;
        }
        if stop.is_cancelled() {
            return Ok(());
        }
        promote::catch_up_voter(context, row.repository_id, stop).await?;
        if let Some(write) = &view.unfinished {
            codeyard_db_placement::cluster::writes::finish_write(pool, &write.id, view.term)
                .await?;
        }
        // The creator enables a fresh empty term only after every selected voter has
        // initialized. A background majority must not race its remaining Initialize RPCs.
        let initializing = view.term == 1
            && view.committed_version == 0
            && view.refs.len() == 1
            && view.refs[0].name == "HEAD";
        if !initializing {
            let terms =
                codeyard_db_placement::cluster::writes::write_family_terms(pool, row.repository_id)
                    .await?;
            codeyard_db_placement::cluster::writes::enable_write_family(pool, &terms).await?;
        }
        return Ok(());
    }
    let primary = row
        .primary_node_id
        .context("the repository has no primary")?;
    let Some(version) =
        codeyard_db_placement::cluster::get_write_version(pool, row.repository_id).await?
    else {
        return Ok(());
    };
    if version > row.applied_version {
        // Downloading the temporary pack cannot change the last usable copy. Transfer
        // invalidates it only immediately before installation, after a complete download.
        let mirror = copy_path(&context.repos_path, uuid);
        crate::replicas::transfer::copy(context, primary, uuid, &mirror, stop).await?;
        // Finish packing before publishing a promotable version: a running maintenance
        // command still needs the mirror directory and prevents relocation.
        maintenance::after_copy(row.repository_id, &mirror).await;
        // Refused if the repository was promoted elsewhere during the copy; the next round
        // copies from the new primary.
        codeyard_db_placement::cluster::set_applied_version(
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

/// Removes the copies this node no longer holds, and moves unowned directories at the primary
/// location into `deleted/`. Copies of its own repositories are kept for `locate`, as are running
/// copies and unplaced primaries. If the database cannot answer, nothing is removed.
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
    for directory in [
        context.repos_path.join(MIRRORS),
        context.repos_path.join(MIRRORS).join(QUARANTINES),
        context.repos_path.clone(),
    ] {
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
            let path = entry.path();
            // Never wait long behind an admitted Git session in the maintenance loop.
            let Ok(guards) = tokio::time::timeout(MOVE_WAIT, async {
                (locks::lock(&path).await, locks::exclusive(&path).await)
            })
            .await
            else {
                continue;
            };
            match codeyard_db_placement::cluster::storage_needed(
                pool,
                &uuid.to_string(),
                context.node_id,
            )
            .await
            {
                Ok(true) => continue,
                Ok(false) => {}
                Err(error) => {
                    warn!(%error, "could not check storage assignment; none removed");
                    return;
                }
            }
            // Rename only. The expiry worker owns recursive deletion outside the
            // takeover loop; its restore window also protects recoverable retired bytes.
            match tokio::task::spawn_blocking(move || {
                let _guards = guards;
                mirror::retire(&path)
            })
            .await
            {
                Ok(Ok(())) => {
                    info!(repository = %uuid, "retired storage this node no longer holds")
                }
                Ok(Err(error)) => {
                    warn!(repository = %uuid, %error, "could not retire unassigned storage")
                }
                Err(error) => warn!(repository = %uuid, %error, "storage retirement stopped"),
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
    for row in codeyard_db_placement::cluster::list_held_replicas(pool, node).await? {
        keep.insert(codeyard_git_storage::wire::repository_uuid(&row.disk_path)?);
    }
    for (_, disk_path) in codeyard_db_placement::cluster::list_primary_paths(pool, node).await? {
        keep.insert(codeyard_git_storage::wire::repository_uuid(&disk_path)?);
    }
    for (_, disk_path) in codeyard_db_placement::cluster::list_unplaced_repositories(pool).await? {
        if let Ok(uuid) = codeyard_git_storage::wire::repository_uuid(&disk_path) {
            keep.insert(uuid);
        }
    }
    Ok(keep)
}

use codeyard_git_storage::cluster::replicas::{
    connected, copy_path, moving, MIRRORS, MOVE_WAIT, QUARANTINES,
};
