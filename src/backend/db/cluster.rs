//! Cluster membership: the `nodes` table and the join tokens that admit new nodes.
use super::models::{
    ClusterNode, HeldReplica, JoinInvite, JoinOutcome, NodeReplicaCounts, NodeStatusChange,
};
use super::DbPool;
use anyhow::Result;
use chrono::{DateTime, Utc};

/// Retains a serving lease on a dedicated connection; closing it releases the advisory lock.
pub struct ServingLock {
    _postgres: Option<sqlx::PgConnection>,
    _file: Option<std::fs::File>,
}

pub async fn serving_lock(pool: &DbPool, identity: &str) -> Result<ServingLock> {
    match pool {
        DbPool::Postgres(pool) => Ok(ServingLock {
            _postgres: Some(super::postgres::cluster::serving_lock(pool, identity).await?),
            _file: None,
        }),
        DbPool::Sqlite(pool) => {
            let mut path = pool.connect_options().get_filename().as_os_str().to_owned();
            path.push(".serve.lock");
            Ok(ServingLock { _postgres: None, _file: Some(crate::backend::locks::exclusive_file(std::path::Path::new(&path))?) })
        }
    }
}

/// Who asks for a membership change. The operator's command line already has database
/// access; a web request acts as an account, which must still be an active administrator
/// when the change commits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Authority<'a> {
    Operator,
    Administrator { user_id: i64, session: &'a str },
}

native_operation!(crate::backend::db::postgres::cluster::has_storage_assignments,
    crate::backend::db::sqlite::cluster::has_storage_assignments,
    fn has_storage_assignments(pool: &DbPool, node: i64) -> Result<bool>);

/// One entry of the declared membership list.
pub struct DeclaredMember {
    pub public_key: String,
    pub name: String,
    pub roles: Vec<String>,
}

/// A primary repository and one optional copy: id, path, holder, applied version, size.
pub type PrimaryReplicaRow = (i64, String, Option<i64>, Option<i64>, i64);

pub struct NewJoinToken<'a> {
    /// Hex SHA-256 of the invite secret; the secret itself is never stored.
    pub token_hash: &'a str,
    pub roles: &'a [String],
    pub reusable: bool,
    pub requires_approval: bool,
    pub lifetime_seconds: i64,
}

pub struct JoinRequest<'a> {
    pub token_hash: &'a str,
    pub public_key: &'a str,
    pub name: &'a str,
    /// Roles the node wants; `None` takes everything the invite allows.
    pub roles: Option<&'a [String]>,
    pub version: &'a str,
}

native_operation!(crate::backend::db::postgres::cluster::create_join_token,
    crate::backend::db::sqlite::cluster::create_join_token,
    fn create_join_token(pool: &DbPool, token: NewJoinToken<'_>) -> Result<()>);

native_operation!(crate::backend::db::postgres::cluster::list_join_tokens,
    crate::backend::db::sqlite::cluster::list_join_tokens,
    /// The invites that can still admit a node, oldest first.
    fn list_join_tokens(pool: &DbPool) -> Result<Vec<JoinInvite>>);

native_operation!(crate::backend::db::postgres::cluster::delete_join_token,
    crate::backend::db::sqlite::cluster::delete_join_token,
    /// Cancels an invite. Nodes that joined with it stay.
    fn delete_join_token(pool: &DbPool, id: i64) -> Result<bool>);

native_operation!(crate::backend::db::postgres::cluster::list_unplaced_repositories,
    crate::backend::db::sqlite::cluster::list_unplaced_repositories,
    /// Repositories with no storage node: id and `disk_path` of each.
    fn list_unplaced_repositories(pool: &DbPool) -> Result<Vec<(i64, String)>>);

native_operation!(crate::backend::db::postgres::cluster::place_repository,
    crate::backend::db::sqlite::cluster::place_repository,
    /// Gives a repository without a storage node to `node_id`. False if it already has one.
    fn place_repository(pool: &DbPool, repo_id: i64, node_id: i64) -> Result<bool>);

native_operation!(crate::backend::db::postgres::cluster::place_fork_family,
    crate::backend::db::sqlite::cluster::place_fork_family,
    fn place_fork_family(pool: &DbPool, repo_id: i64, node_id: i64, local: &[i64]) -> Result<Vec<i64>>);

native_operation!(crate::backend::db::postgres::cluster::join_with_token,
    crate::backend::db::sqlite::cluster::join_with_token,
    /// Redeems a join token for a new node in one transaction: the token's use is
    /// taken and the node row is added together, or neither happens.
    fn join_with_token(pool: &DbPool, request: JoinRequest<'_>) -> Result<JoinOutcome>);

native_operation!(crate::backend::db::postgres::cluster::register_own_node,
    crate::backend::db::sqlite::cluster::register_own_node,
    /// A node with database access adds itself as `active` if it is not known yet.
    /// A node that already has a row keeps its roles and status; only `version` is
    /// refreshed. Registering is not a sign of life: heartbeats record that.
    /// Returns the row as it now stands.
    fn register_own_node(
    pool: &DbPool,
    public_key: &str,
    name: &str,
    roles: &[String],
    version: &str,
) -> Result<ClusterNode>);

native_operation!(crate::backend::db::postgres::cluster::upsert_declared_nodes,
    crate::backend::db::sqlite::cluster::upsert_declared_nodes,
    /// Applies the declared membership list in one transaction: if one entry fails, none
    /// is applied. Each entry sets name and roles and makes the node `active`. A `revoked`
    /// node stays revoked; the returned rows show that.
    fn upsert_declared_nodes(pool: &DbPool, nodes: &[DeclaredMember]) -> Result<Vec<ClusterNode>>);

native_operation!(crate::backend::db::postgres::cluster::get_node_by_public_key,
    crate::backend::db::sqlite::cluster::get_node_by_public_key,
    fn get_node_by_public_key(pool: &DbPool, public_key: &str) -> Result<Option<ClusterNode>>);

native_operation!(crate::backend::db::postgres::cluster::get_node_by_name,
    crate::backend::db::sqlite::cluster::get_node_by_name,
    fn get_node_by_name(pool: &DbPool, name: &str) -> Result<Option<ClusterNode>>);

native_operation!(crate::backend::db::postgres::cluster::list_nodes,
    crate::backend::db::sqlite::cluster::list_nodes,
    fn list_nodes(pool: &DbPool) -> Result<Vec<ClusterNode>>);

native_operation!(crate::backend::db::postgres::cluster::is_active_node,
    crate::backend::db::sqlite::cluster::is_active_node,
    /// The check made for every incoming node connection.
    fn is_active_node(pool: &DbPool, public_key: &str) -> Result<bool>);

native_operation!(crate::backend::db::postgres::cluster::set_node_status,
    crate::backend::db::sqlite::cluster::set_node_status,
    /// Makes the named node `active` (approve) or `revoked`. An administrator must still be
    /// active when the change commits: the check and the write share one transaction.
    fn set_node_status(pool: &DbPool, authority: Authority<'_>, name: &str, status: &str) -> Result<NodeStatusChange>);

native_operation!(crate::backend::db::postgres::cluster::record_node_heartbeat,
    crate::backend::db::sqlite::cluster::record_node_heartbeat,
    /// Records a sign of life. Returns false if no node has this key.
    fn record_node_heartbeat(
    pool: &DbPool,
    public_key: &str,
    version: &str,
    addr_hint: &str,
    disk_free: Option<i64>,
) -> Result<bool>);

native_operation!(crate::backend::db::postgres::cluster::touch_node,
    crate::backend::db::sqlite::cluster::touch_node,
    /// Records a sign of life for a node that has no heartbeat of its own, such as a worker
    /// without a database, when it makes an authenticated request.
    fn touch_node(pool: &DbPool, public_key: &str) -> Result<()>);

native_operation!(crate::backend::db::postgres::cluster::clear_node_heartbeat,
    crate::backend::db::sqlite::cluster::clear_node_heartbeat,
    /// Stops counting the node as live until its next heartbeat. Called when it shuts down.
    fn clear_node_heartbeat(pool: &DbPool, public_key: &str) -> Result<()>);

native_operation!(crate::backend::db::postgres::cluster::get_node_by_id,
    crate::backend::db::sqlite::cluster::get_node_by_id,
    fn get_node_by_id(pool: &DbPool, id: i64) -> Result<Option<ClusterNode>>);

native_operation!(crate::backend::db::postgres::cluster::promote_manual_family,
    crate::backend::db::sqlite::cluster::promote_manual_family,
    fn promote_manual_family(pool: &DbPool, repository: i64, from: i64, to: i64) -> Result<FamilyOutcome>);
native_operation!(crate::backend::db::postgres::cluster::storage_needed,
    crate::backend::db::sqlite::cluster::storage_needed,
    fn storage_needed(pool: &DbPool, uuid: &str, node: i64) -> Result<bool>);
native_operation!(crate::backend::db::postgres::cluster::list_primary_paths,
    crate::backend::db::sqlite::cluster::list_primary_paths,
    fn list_primary_paths(pool: &DbPool, node: i64) -> Result<Vec<(i64, String)>>);
native_operation!(crate::backend::db::postgres::cluster::read_candidates,
    crate::backend::db::sqlite::cluster::read_candidates,
    fn read_candidates(pool: &DbPool, uuid: &str, previous: i64) -> Result<Vec<(i64, bool)>>);
native_operation!(crate::backend::db::postgres::cluster::replica_applied_version,
    crate::backend::db::sqlite::cluster::replica_applied_version,
    fn replica_applied_version(pool: &DbPool, repository: i64, node: i64) -> Result<Option<i64>>);
native_operation!(crate::backend::db::postgres::cluster::get_repository_by_uuid,
    crate::backend::db::sqlite::cluster::get_repository_by_uuid,
    fn get_repository_by_uuid(pool: &DbPool, uuid: &str) -> Result<Option<super::models::Repository>>);

/// What a promotion did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromoteOutcome {
    /// `to` is the primary now. It held `applied_version` of the `write_version` writes.
    Promoted {
        applied_version: i64,
        write_version: i64,
    },
    /// `to` holds a copy that is behind, and the promotion required a current one.
    Stale {
        applied_version: i64,
        write_version: i64,
    },
    /// `to` holds no copy of the repository.
    NoReplica,
    /// `to` is not an active node that is reporting in, so nothing changed.
    TargetOffline,
    /// The primary is no longer `from`, so nothing changed.
    Moved,
}

/// What a whole-family promotion did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FamilyOutcome {
    /// Every member is on `to` now: their ids, lowest first.
    Promoted(Vec<i64>),
    /// A member is no longer on `from`, or `to` lacks a current copy of one. Nothing changed.
    Refused,
}

native_operation!(crate::backend::db::postgres::cluster::get_replicas,
    crate::backend::db::sqlite::cluster::get_replicas,
    /// The replica setting as stored: `1` (the default), a count, or `all`.
    fn get_replicas(pool: &DbPool) -> Result<String>);

native_operation!(crate::backend::db::postgres::cluster::set_replicas,
    crate::backend::db::sqlite::cluster::set_replicas,
    /// Stores the replica setting. False if the installation has no configuration row yet.
    fn set_replicas(pool: &DbPool, value: &str) -> Result<bool>);

native_operation!(crate::backend::db::postgres::cluster::cluster_enabled,
    crate::backend::db::sqlite::cluster::cluster_enabled,
    /// Whether `codeyard cluster enable` turned on cluster mode. Single-node mode, the
    /// default, allows only the server and runners.
    fn cluster_enabled(pool: &DbPool) -> Result<bool>);

native_operation!(crate::backend::db::postgres::cluster::enable_cluster,
    crate::backend::db::sqlite::cluster::enable_cluster,
    /// Turns on cluster mode. Returns false when it was already on.
    fn enable_cluster(pool: &DbPool) -> Result<bool>);

native_operation!(crate::backend::db::postgres::cluster::is_admitted_node,
    crate::backend::db::sqlite::cluster::is_admitted_node,
    /// Whether this key may use the cluster transport: an active node whose roles the
    /// current mode allows. Single-node mode admits only runners.
    fn is_admitted_node(pool: &DbPool, public_key: &str) -> Result<bool>);

native_operation!(crate::backend::db::postgres::cluster::bump_write_version,
    crate::backend::db::sqlite::cluster::bump_write_version,
    /// Counts one ref change on a repository's primary, if `node_id` holds the repository
    /// (`None` accepts any holder). Call it before Git makes the change. Returns `false`
    /// when another node holds the repository. A repository that no storage node holds has
    /// no replicas and counts nothing.
    fn bump_write_version(pool: &DbPool, repo_id: i64, node_id: Option<Option<i64>>) -> Result<bool>);

native_operation!(crate::backend::db::postgres::cluster::get_write_version,
    crate::backend::db::sqlite::cluster::get_write_version,
    fn get_write_version(pool: &DbPool, repo_id: i64) -> Result<Option<i64>>);

native_operation!(crate::backend::db::postgres::cluster::list_primary_repositories,
    crate::backend::db::sqlite::cluster::list_primary_repositories,
    /// Repositories whose primary is `node_id`: id, disk path, and one row per copy with
    /// the node that holds it and the version that copy has applied (`None` for both when
    /// there are no copies).
    fn list_primary_repositories(pool: &DbPool, node_id: i64) -> Result<Vec<PrimaryReplicaRow>>);

native_operation!(crate::backend::db::postgres::cluster::pending_replica_bytes,
    crate::backend::db::sqlite::cluster::pending_replica_bytes,
    fn pending_replica_bytes(pool: &DbPool) -> Result<Vec<(i64, i64)>>);

native_operation!(crate::backend::db::postgres::cluster::list_held_replicas,
    crate::backend::db::sqlite::cluster::list_held_replicas,
    /// The copies that `node_id` holds.
    fn list_held_replicas(pool: &DbPool, node_id: i64) -> Result<Vec<HeldReplica>>);

native_operation!(crate::backend::db::postgres::cluster::list_replicas,
    crate::backend::db::sqlite::cluster::list_replicas,
    /// The copies of one repository as (node id, applied version).
    fn list_replicas(pool: &DbPool, repo_id: i64) -> Result<Vec<(i64, i64)>>);

native_operation!(crate::backend::db::postgres::cluster::list_fork_family,
    crate::backend::db::sqlite::cluster::list_fork_family,
    /// The fork family of `repo_id`: the repository, its source, its forks and their forks,
    /// linked through `forked_from_id`. Each as (id, primary node, write version), by id.
    fn list_fork_family(pool: &DbPool, repo_id: i64) -> Result<Vec<(i64, Option<i64>, i64)>>);

native_operation!(crate::backend::db::postgres::cluster::insert_replica,
    crate::backend::db::sqlite::cluster::insert_replica,
    /// Adds a copy on `node_id`, only while `primary_id` is still the repository's primary
    /// and `node_id` is not that primary. False if nothing was added.
    fn insert_replica(pool: &DbPool, repo_id: i64, node_id: i64, primary_id: i64) -> Result<bool>);

native_operation!(crate::backend::db::postgres::cluster::delete_replica,
    crate::backend::db::sqlite::cluster::delete_replica,
    /// Retires a copy only while `primary_id` is still primary, serialized with promotion.
    fn delete_replica(pool: &DbPool, repo_id: i64, node_id: i64, primary_id: i64) -> Result<bool>);

native_operation!(crate::backend::db::postgres::cluster::set_applied_version,
    crate::backend::db::sqlite::cluster::set_applied_version,
    /// Records the primary version a copy now holds, and when. The copy was taken from
    /// `primary_id`; nothing is recorded if the repository has moved to another primary
    /// since, because that copy holds the old primary's refs.
    fn set_applied_version(pool: &DbPool, repo_id: i64, node_id: i64, primary_id: i64, version: i64) -> Result<bool>);

native_operation!(crate::backend::db::postgres::cluster::replica_counts,
    crate::backend::db::sqlite::cluster::replica_counts,
    fn replica_counts(pool: &DbPool) -> Result<Vec<NodeReplicaCounts>>);

native_operation!(crate::backend::db::postgres::cluster::promote_repository,
    crate::backend::db::sqlite::cluster::promote_repository,
    /// Makes `to` the primary if `from` still is, in one transaction. `from` keeps a copy
    /// with nothing applied, and no other copy counts as holding more than `to` does.
    /// `to` must be an active node that is reporting in and hold a completed copy; with
    /// `require_current`, the copy must hold every write.
    fn promote_repository(pool: &DbPool, repo_id: i64, from: i64, to: i64, require_current: bool) -> Result<PromoteOutcome>);

native_operation!(crate::backend::db::postgres::cluster::promote_fork_family,
    crate::backend::db::sqlite::cluster::promote_fork_family,
    /// Moves the whole fork family that `repo_id` belongs to from `from` to `to`, in one
    /// transaction. Refused, and nothing changed, unless every member is still on `from` and
    /// `to` is active and reporting in and holds a current copy of each, and `from` is still
    /// active but offline.
    fn promote_fork_family(pool: &DbPool, repo_id: i64, from: i64, to: i64) -> Result<FamilyOutcome>);
