//! Replicas: copies of a repository on other storage nodes. The database rules run on
//! both engines; the copying runs between real in-process nodes on PostgreSQL.
#![cfg(all(not(target_arch = "wasm32"), target_os = "linux"))]
#[path = "support/pull_requests.rs"]
mod http;
#[path = "support/native_database.rs"]
mod native;

use codeyard::backend::cli::cluster::{self as cluster_cli, Command};
use codeyard::backend::{
    api::create_router,
    app::AppState,
    ci::CiConfig,
    cluster::{
        self,
        replicas::{self, Copying, Promotion, Retry, Silent},
        rpc::{self, Failure, Operation, Request},
        NodeKey,
    },
    db::{
        self,
        cluster::{FamilyOutcome, PromoteOutcome},
        models::{NodeReplicaCounts, ReplicaSetting, Repository},
        DbPool,
    },
    repositories::store::{
        repo_store, ErrorKind, FileWrite, GitService, GitSession, GitStreams, RepoError, RepoStore,
    },
    storage::ObjectStore,
};
use rand::RngCore;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    str::FromStr,
    sync::Arc,
    time::Duration,
};
use tokio::sync::watch;
use uuid::Uuid;

#[test]
fn replica_setting_accepts_counts_and_all_and_nothing_else() {
    for (text, setting) in [
        ("1", ReplicaSetting::Copies(1)),
        ("2", ReplicaSetting::Copies(2)),
        ("999", ReplicaSetting::Copies(999)),
        ("all", ReplicaSetting::All),
    ] {
        assert_eq!(ReplicaSetting::from_str(text).unwrap(), setting, "{text}");
        assert_eq!(setting.to_string(), text);
    }
    for text in ["", "0", "01", "1000", "-1", "two", "ALL", " 2", "2 "] {
        assert!(ReplicaSetting::from_str(text).is_err(), "{text:?}");
    }
}

/// Registers an active storage node with a fresh key.
async fn storage_node(pool: &DbPool, name: &str) -> i64 {
    db::cluster::register_own_node(
        pool,
        &NodeKey::generate().public_hex(),
        name,
        &["storage".to_owned()],
        "test",
    )
    .await
    .unwrap()
    .id
}

/// Registers an active storage node that is reporting in, so it can be a promotion target.
async fn reporting_storage_node(pool: &DbPool, name: &str) -> i64 {
    let (node, key) = keyed_storage_node(pool, name).await;
    report_in(pool, &key).await;
    node
}

/// A repository whose primary is `primary`, at version zero.
async fn placed_repository(pool: &DbPool, primary: i64) -> i64 {
    let owner = db::users::accounts::create_user(pool, &format!("owner{}", primary), None)
        .await
        .unwrap();
    let disk_path = format!("/srv/repos/{}.git", Uuid::new_v4());
    let repository = db::repositories::records::create_repository(
        pool,
        owner.id,
        "replicated",
        None,
        false,
        &disk_path,
    )
    .await
    .unwrap();
    assert!(db::cluster::place_repository(pool, repository.id, primary)
        .await
        .unwrap());
    repository.id
}

/// The rules every engine must keep: the setting, versions, copies, promotion and counts.
async fn check_replica_rules(pool: &DbPool) {
    db::node_config::upsert_node_configuration(pool, "public", true)
        .await
        .unwrap();
    assert_eq!(db::cluster::get_replicas(pool).await.unwrap(), "1");
    assert!(db::cluster::set_replicas(pool, "all").await.unwrap());
    assert_eq!(db::cluster::get_replicas(pool).await.unwrap(), "all");
    assert!(db::cluster::set_replicas(pool, "2").await.unwrap());
    for refused in ["0", "1000", "two", "02"] {
        assert!(
            db::cluster::set_replicas(pool, refused).await.is_err(),
            "{refused}"
        );
    }
    assert_eq!(db::cluster::get_replicas(pool).await.unwrap(), "2");

    let a = storage_node(pool, "store-a").await;
    let b = reporting_storage_node(pool, "store-b").await;
    let c = storage_node(pool, "store-c").await;
    let repo = placed_repository(pool, a).await;
    assert_eq!(
        db::cluster::get_write_version(pool, repo).await.unwrap(),
        Some(0)
    );
    db::cluster::bump_write_version(pool, repo).await.unwrap();
    db::cluster::bump_write_version(pool, repo).await.unwrap();
    assert_eq!(
        db::cluster::get_write_version(pool, repo).await.unwrap(),
        Some(2)
    );
    assert!(db::cluster::bump_write_version(pool, repo + 1000)
        .await
        .is_err());
    // A repository that no storage node holds has no replicas, so nothing is counted.
    let owner = db::users::accounts::create_user(pool, "unplaced-owner", None)
        .await
        .unwrap();
    let unplaced = db::repositories::records::create_repository(
        pool,
        owner.id,
        "unplaced",
        None,
        false,
        &format!("/srv/repos/{}.git", Uuid::new_v4()),
    )
    .await
    .unwrap();
    db::cluster::bump_write_version(pool, unplaced.id)
        .await
        .unwrap();
    assert_eq!(
        db::cluster::get_write_version(pool, unplaced.id)
            .await
            .unwrap(),
        Some(0)
    );

    // Only the primary adds copies, and a primary never holds a copy of itself.
    assert!(db::cluster::insert_replica(pool, repo, b, a).await.unwrap());
    assert!(!db::cluster::insert_replica(pool, repo, b, a).await.unwrap());
    assert!(!db::cluster::insert_replica(pool, repo, a, a).await.unwrap());
    assert!(!db::cluster::insert_replica(pool, repo, c, b).await.unwrap());
    assert!(db::cluster::insert_replica(pool, repo, c, a).await.unwrap());
    assert_eq!(
        db::cluster::list_replicas(pool, repo).await.unwrap(),
        vec![(b, -1), (c, -1)]
    );

    assert!(!db::cluster::delete_replica(pool, repo, b, c).await.unwrap());
    assert!(db::cluster::delete_replica(pool, repo, c, a).await.unwrap());
    assert!(!db::cluster::delete_replica(pool, repo, c, a).await.unwrap());
    assert_eq!(
        db::cluster::promote_repository(pool, repo, a, c, false)
            .await
            .unwrap(),
        PromoteOutcome::NoReplica
    );
    assert!(db::cluster::insert_replica(pool, repo, c, a).await.unwrap());

    db::cluster::set_applied_version(pool, repo, b, a, 2)
        .await
        .unwrap();
    let held = db::cluster::list_held_replicas(pool, b).await.unwrap();
    assert_eq!(held.len(), 1);
    assert_eq!(held[0].repository_id, repo);
    assert_eq!(held[0].primary_node_id, Some(a));
    assert_eq!((held[0].applied_version, held[0].write_version), (2, 2));
    let primaries = db::cluster::list_primary_repositories(pool, a)
        .await
        .unwrap();
    assert_eq!(
        primaries.iter().map(|row| row.2).collect::<Vec<_>>(),
        vec![Some(b), Some(c)]
    );

    let counts = db::cluster::replica_counts(pool).await.unwrap();
    let of = |node: i64| {
        counts
            .iter()
            .find(|row| row.node_id == node)
            .copied()
            .unwrap()
    };
    assert_eq!(
        of(a),
        NodeReplicaCounts {
            node_id: a,
            primaries: 1,
            replicas: 0,
            current: 0
        }
    );
    assert_eq!(
        (of(b).replicas, of(b).current),
        (1, 1),
        "copy at version 2 of 2"
    );
    assert_eq!(
        (of(c).replicas, of(c).current),
        (1, 0),
        "copy with nothing applied"
    );

    // Promotion needs the primary to be where the caller thinks it is, and a copy.
    let promote = |from, to, require_current| {
        db::cluster::promote_repository(pool, repo, from, to, require_current)
    };
    assert_eq!(promote(b, c, false).await.unwrap(), PromoteOutcome::Moved);
    assert_eq!(
        promote(a, a + 1000, false).await.unwrap(),
        PromoteOutcome::NoReplica
    );
    for require_current in [true, false] {
        assert_eq!(
            promote(a, c, require_current).await.unwrap(),
            PromoteOutcome::Stale {
                applied_version: -1,
                write_version: 2
            }
        );
    }
    assert_eq!(
        promote(a, b, true).await.unwrap(),
        PromoteOutcome::Promoted {
            applied_version: 2,
            write_version: 2
        }
    );
    // A plan read by the former primary cannot retire copies after promotion.
    assert!(!db::cluster::delete_replica(pool, repo, c, a).await.unwrap());
    // `a` keeps a copy with nothing applied; `b` no longer holds one.
    assert_eq!(
        db::cluster::list_replicas(pool, repo).await.unwrap(),
        vec![(a, -1), (c, -1)]
    );
    assert_eq!(
        db::cluster::get_write_version(pool, repo).await.unwrap(),
        Some(2),
        "promotion keeps the version"
    );
    assert!(db::cluster::list_held_replicas(pool, b)
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn retirement_waits_for_single_and_family_promotions() {
    let database = native::PostgresDatabase::new().await;
    let pool = &database.pool;
    let raw = sqlx::PgPool::connect(&database.connection_url())
        .await
        .unwrap();
    let a = storage_node(pool, "retire-a").await;
    let b = reporting_storage_node(pool, "retire-b").await;
    let c = storage_node(pool, "retire-c").await;
    for family in [false, true] {
        let repo = family_member(pool, &format!("retire{family}"), a, None).await;
        add_copy(pool, repo, b, a, 0).await;
        add_copy(pool, repo, c, a, 0).await;
        // Pause promotion after it has read b's version, while updating another copy.
        let mut hold = raw.begin().await.unwrap();
        let holder: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *hold)
            .await
            .unwrap();
        sqlx::query("SELECT 1 FROM repository_replicas WHERE repository_id = $1 AND node_id = $2 FOR UPDATE")
            .bind(repo).bind(c).fetch_one(&mut *hold).await.unwrap();
        let promotion_pool = pool.clone();
        let promotion = tokio::spawn(async move {
            if family {
                assert!(matches!(
                    db::cluster::promote_fork_family(&promotion_pool, repo, a, b)
                        .await
                        .unwrap(),
                    FamilyOutcome::Promoted(_)
                ));
            } else {
                assert!(matches!(
                    db::cluster::promote_repository(&promotion_pool, repo, a, b, true)
                        .await
                        .unwrap(),
                    PromoteOutcome::Promoted { .. }
                ));
            }
        });
        let promoter: i32 = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Some(pid) = sqlx::query_scalar(
                    "SELECT pid FROM pg_stat_activity WHERE $1 = ANY(pg_blocking_pids(pid))",
                )
                .bind(holder)
                .fetch_optional(&raw)
                .await
                .unwrap()
                {
                    break pid;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("promotion waits after reading the target copy");
        let retire_pool = pool.clone();
        let retirement = tokio::spawn(async move {
            db::cluster::delete_replica(&retire_pool, repo, b, a)
                .await
                .unwrap()
        });
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let waiting: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE $1 = ANY(pg_blocking_pids(pid)))")
                    .bind(promoter).fetch_one(&raw).await.unwrap();
                if waiting || retirement.is_finished() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }).await.expect("retirement reaches the repository lock");
        assert!(
            !retirement.is_finished(),
            "retirement must not delete a copy promotion already read"
        );
        assert!(db::cluster::list_replicas(pool, repo)
            .await
            .unwrap()
            .contains(&(b, 0)));
        hold.commit().await.unwrap();
        promotion.await.unwrap();
        assert!(
            !retirement.await.unwrap(),
            "the former primary's plan is now stale"
        );
        assert_eq!(primary_of(pool, repo).await, b);
    }
    raw.close().await;
}

#[tokio::test]
async fn replica_rules_hold_on_sqlite() {
    let database = native::NativeDatabase::new().await;
    check_replica_rules(&database.pool).await;
}

#[tokio::test]
async fn replica_rules_hold_on_postgres() {
    let database = native::PostgresDatabase::new().await;
    check_replica_rules(&database.pool).await;
}

fn settings(name: &str) -> cluster::Settings {
    cluster::Settings::from_lookup(&|key| match key {
        "IROH_BIND" => Some("127.0.0.1:0".into()),
        "NODE_NAME" => Some(name.into()),
        _ => None,
    })
    .unwrap()
    .unwrap()
}

/// A storage node with its own state and repositories directory, online and reporting.
async fn start_storage(pool: &DbPool, root: &Path, name: &str) -> cluster::Cluster {
    start_node(pool, root, name, "storage").await
}

async fn start_node(pool: &DbPool, root: &Path, name: &str, roles: &str) -> cluster::Cluster {
    let node = cluster::start(
        pool,
        &root.join(format!("{name}-state")),
        &root.join(format!("{name}-repos")),
        &roles.parse().unwrap(),
        settings(name),
    )
    .await
    .unwrap();
    node.publish().await.unwrap();
    node
}

/// A repository on a primary, written through the store as its owner.
struct Repo<'a> {
    pool: &'a DbPool,
    store: RepoStore,
    repository: Repository,
    actor: i64,
}

impl Repo<'_> {
    fn id(&self) -> i64 {
        self.repository.id
    }

    /// Commits `content` at `path` on `branch`, which must be at `expected`. Returns the
    /// new commit.
    async fn save(&self, branch: &str, path: &str, expected: &str, content: &[u8]) -> String {
        let write = FileWrite {
            branch: branch.into(),
            path: path.into(),
            message: format!("add {path}"),
            expected_revision: expected.into(),
            creating: true,
            deleting: false,
        };
        self.store
            .save_file_as(
                self.pool,
                self.id(),
                self.actor,
                &write,
                content,
                "Author",
                "author@example.invalid",
            )
            .await
            .unwrap()
            .sha
    }

    async fn branch(&self, name: &str, sha: &str) {
        self.store
            .create_branch_as(self.pool, self.id(), self.actor, name, sha)
            .await
            .unwrap();
    }

    async fn delete_branch(&self, name: &str) {
        self.store
            .delete_branch_as(self.pool, self.id(), self.actor, name)
            .await
            .unwrap();
    }

    async fn tag(&self, name: &str, sha: &str) {
        self.store
            .create_tag_as(
                self.pool,
                self.id(),
                self.actor,
                name,
                sha,
                None,
                "Tester",
                "tester@example.invalid",
            )
            .await
            .unwrap();
    }

    async fn delete_tag(&self, name: &str) {
        self.store
            .delete_tag_as(self.pool, self.id(), self.actor, name)
            .await
            .unwrap();
    }
}

/// An empty repository on `primary`'s disk, with its row placed on `primary`.
async fn create_repository<'a>(
    pool: &'a DbPool,
    primary: &cluster::Cluster,
    name: &str,
) -> Repo<'a> {
    let owner = db::users::accounts::create_user(pool, &format!("{name}_owner"), None)
        .await
        .unwrap();
    let store = RepoStore::create(&primary.context.repos_path, "main", None)
        .await
        .unwrap();
    let row = db::repositories::records::create_repository(
        pool,
        owner.id,
        name,
        None,
        false,
        store.location(),
    )
    .await
    .unwrap();
    let placed = db::cluster::place_repository(pool, row.id, primary.context.node_id)
        .await
        .unwrap();
    assert!(placed);
    let repository = db::repositories::records::get_repository_by_id(pool, row.id)
        .await
        .unwrap()
        .unwrap();
    Repo {
        pool,
        store: repo_store(&repository, Some(&primary.context)),
        repository,
        actor: owner.id,
    }
}

/// Where a primary lives on `node`, and where a copy of it lives.
fn primary_path(node: &cluster::Cluster, uuid: Uuid) -> PathBuf {
    node.context.repos_path.join(format!("{uuid}.git"))
}

fn copy_path(node: &cluster::Cluster, uuid: Uuid) -> PathBuf {
    node.context
        .repos_path
        .join("mirrors")
        .join(format!("{uuid}.git"))
}

/// The copy's applied version on `node`, or `None` when the node has no copy row.
async fn applied_on(pool: &DbPool, repository: i64, node: &cluster::Cluster) -> Option<i64> {
    db::cluster::list_replicas(pool, repository)
        .await
        .unwrap()
        .into_iter()
        .find(|(id, _)| *id == node.context.node_id)
        .map(|(_, applied)| applied)
}

async fn pass(node: &cluster::Cluster) {
    replicas::pass(
        &node.context,
        &mut Retry::default(),
        &mut Silent::default(),
        &mut Copying::default(),
    )
    .await
    .unwrap();
}

/// Both nodes run one pass, the primary's first. Then the copy is current: same refs and
/// HEAD as the primary, and its applied version is the repository's write version.
async fn assert_current(primary: &cluster::Cluster, copy: &cluster::Cluster, repo: &Repo<'_>) {
    pass(primary).await;
    pass(copy).await;
    let uuid = rpc::repository_uuid(&repo.repository.disk_path).unwrap();
    let primary_store = RepoStore::from_disk_path(primary_path(primary, uuid).to_str().unwrap());
    let copy_store = RepoStore::from_disk_path(copy_path(copy, uuid).to_str().unwrap());
    assert_eq!(
        copy_store.mirror_refs().await.unwrap(),
        primary_store.mirror_refs().await.unwrap(),
        "the copy has every ref of the primary"
    );
    assert_eq!(
        copy_store.head_target().await.unwrap(),
        primary_store.head_target().await.unwrap(),
        "the copy's HEAD names the primary's branch"
    );
    let version = db::cluster::get_write_version(repo.pool, repo.id())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        applied_on(repo.pool, repo.id(), copy).await,
        Some(version),
        "the copy is current"
    );
}

/// A commit with no parent and an empty tree, written to the object store only.
fn orphan_commit(path: &Path, message: &str) -> String {
    let repo = git2::Repository::open_bare(path).unwrap();
    let tree = repo
        .find_tree(repo.treebuilder(None).unwrap().write().unwrap())
        .unwrap();
    let signature = git2::Signature::now("Tester", "tester@example.invalid").unwrap();
    repo.commit(None, &signature, &signature, message, &tree, &[])
        .unwrap()
        .to_string()
}

async fn two_storage_nodes(pool: &DbPool, root: &Path) -> (cluster::Cluster, cluster::Cluster) {
    db::node_config::upsert_node_configuration(pool, "public", true)
        .await
        .unwrap();
    assert!(db::cluster::set_replicas(pool, "2").await.unwrap());
    let primary = start_storage(pool, root, "store-a").await;
    let copy = start_storage(pool, root, "store-b").await;
    (primary, copy)
}

#[tokio::test]
async fn copies_follow_commits_tags_branches_and_force_moves() {
    let database = native::PostgresDatabase::new().await;
    let root = tempfile::tempdir().unwrap();
    let (primary, copy) = two_storage_nodes(&database.pool, root.path()).await;
    let repo = create_repository(&database.pool, &primary, "copied").await;
    let uuid = rpc::repository_uuid(&repo.repository.disk_path).unwrap();

    let first = repo
        .save("main", "README.md", "unborn", b"# copied\n")
        .await;
    assert_current(&primary, &copy, &repo).await;
    repo.save("main", "src/lib.rs", &first, b"pub fn one() {}\n")
        .await;
    assert_current(&primary, &copy, &repo).await;
    repo.tag("v1", &first).await;
    assert_current(&primary, &copy, &repo).await;
    repo.branch("feature", &first).await;
    assert_current(&primary, &copy, &repo).await;

    // A pull request head lives under refs/pull, and a promoted copy needs it.
    repo.store
        .fetch_pull_head(
            repo.pool,
            repo.id(),
            repo.id(),
            repo.actor,
            &repo.store,
            1,
            &first,
            None,
        )
        .await
        .unwrap();
    assert_current(&primary, &copy, &repo).await;
    let copied = RepoStore::from_disk_path(copy_path(&copy, uuid).to_str().unwrap());
    assert_eq!(
        copied.mirror_refs().await.unwrap()["refs/pull/1/head"],
        first
    );

    // A force move: the branch goes to history that does not contain its old tip.
    let orphan = orphan_commit(&primary_path(&primary, uuid), "unrelated history");
    repo.delete_branch("feature").await;
    repo.branch("feature", &orphan).await;
    assert_current(&primary, &copy, &repo).await;
    let copied = RepoStore::from_disk_path(copy_path(&copy, uuid).to_str().unwrap());
    assert_eq!(copied.refs().await.unwrap()["refs/heads/feature"], orphan);

    repo.delete_tag("v1").await;
    assert_current(&primary, &copy, &repo).await;
    repo.delete_branch("feature").await;
    assert_current(&primary, &copy, &repo).await;

    copy.shutdown().await;
    primary.shutdown().await;
}

#[tokio::test]
async fn empty_repositories_and_large_files_copy() {
    let database = native::PostgresDatabase::new().await;
    let root = tempfile::tempdir().unwrap();
    let (primary, copy) = two_storage_nodes(&database.pool, root.path()).await;
    let repo = create_repository(&database.pool, &primary, "empty").await;
    let uuid = rpc::repository_uuid(&repo.repository.disk_path).unwrap();

    // No refs yet: the copy is created empty, with the same HEAD.
    assert!(repo.store.refs().await.unwrap().is_empty());
    assert_current(&primary, &copy, &repo).await;
    assert!(copy_path(&copy, uuid).is_dir());

    // 20 MiB of random bytes in one commit. An editor save is capped at 1 MiB, so the
    // commit is written with Git and then counted as a push would be.
    let mut content = vec![0u8; 20 * 1024 * 1024];
    rand::rngs::OsRng.fill_bytes(&mut content);
    let git = git2::Repository::open_bare(primary_path(&primary, uuid)).unwrap();
    let blob = git.blob(&content).unwrap();
    let mut tree = git.treebuilder(None).unwrap();
    tree.insert("big.bin", blob, 0o100644).unwrap();
    let tree = git.find_tree(tree.write().unwrap()).unwrap();
    let signature = git2::Signature::now("Tester", "tester@example.invalid").unwrap();
    git.commit(
        Some("refs/heads/main"),
        &signature,
        &signature,
        "large file",
        &tree,
        &[],
    )
    .unwrap();
    db::cluster::bump_write_version(repo.pool, repo.id())
        .await
        .unwrap();
    assert_current(&primary, &copy, &repo).await;
    let copied = RepoStore::from_disk_path(copy_path(&copy, uuid).to_str().unwrap());
    let tree = copied.tree("main", "").await.unwrap();
    assert_eq!(tree.len(), 1);
    assert_eq!(tree[0].size, Some(content.len() as u64));

    copy.shutdown().await;
    primary.shutdown().await;
}

#[tokio::test]
async fn only_nodes_holding_a_copy_may_ask_for_its_packs() {
    let database = native::PostgresDatabase::new().await;
    let root = tempfile::tempdir().unwrap();
    let (primary, holder) = two_storage_nodes(&database.pool, root.path()).await;
    let repo = create_repository(&database.pool, &primary, "guarded").await;
    let uuid = rpc::repository_uuid(&repo.repository.disk_path).unwrap();
    repo.save("main", "a.txt", "unborn", b"a").await;
    // Assign the copy while there is only one eligible holder.
    assert_current(&primary, &holder, &repo).await;
    let outsider = start_storage(&database.pool, root.path(), "store-c").await;
    assert_eq!(applied_on(&database.pool, repo.id(), &outsider).await, None);

    let pack_request = || Request {
        repository: uuid,
        operation: Operation::ReplicaPack { have: Vec::new() },
        actor: None,
        ci_run: None,
    };
    let refused = outsider
        .context
        .git_session(
            primary.context.node_id,
            pack_request(),
            GitStreams {
                input: &mut tokio::io::empty(),
                output: &mut tokio::io::sink(),
                errors: &mut tokio::io::sink(),
            },
        )
        .await;
    assert!(
        refused.is_err(),
        "a storage node without a copy row is refused"
    );
    let (_, status) = holder
        .context
        .git_session(
            primary.context.node_id,
            pack_request(),
            GitStreams {
                input: &mut tokio::io::empty(),
                output: &mut tokio::io::sink(),
                errors: &mut tokio::io::sink(),
            },
        )
        .await
        .unwrap();
    assert!(status.success(), "the copy holder gets its pack");

    outsider.shutdown().await;
    holder.shutdown().await;
    primary.shutdown().await;
}

#[tokio::test]
async fn the_setting_decides_how_many_copies_exist_and_removes_the_rest() {
    let database = native::PostgresDatabase::new().await;
    let pool = &database.pool;
    db::node_config::upsert_node_configuration(pool, "public", true)
        .await
        .unwrap();
    let root = tempfile::tempdir().unwrap();
    let primary = start_storage(pool, root.path(), "store-a").await;
    let first = start_storage(pool, root.path(), "store-b").await;
    let second = start_storage(pool, root.path(), "store-c").await;
    let repo = create_repository(pool, &primary, "counted").await;
    let uuid = rpc::repository_uuid(&repo.repository.disk_path).unwrap();
    repo.save("main", "a.txt", "unborn", b"a").await;
    let has_copy = |node: &cluster::Cluster| copy_path(node, uuid).exists();

    assert!(db::cluster::set_replicas(pool, "all").await.unwrap());
    pass(&primary).await;
    pass(&first).await;
    pass(&second).await;
    assert!(
        has_copy(&first) && has_copy(&second),
        "all: every storage node has a copy"
    );

    assert!(db::cluster::set_replicas(pool, "1").await.unwrap());
    pass(&primary).await;
    pass(&first).await;
    pass(&second).await;
    assert!(
        !has_copy(&first) && !has_copy(&second),
        "1: the copies are removed"
    );
    assert_eq!(applied_on(pool, repo.id(), &first).await, None);
    assert!(
        primary_path(&primary, uuid).exists(),
        "the primary is never removed"
    );

    // Stop live disk samples from replacing the tied placement inputs.
    for node in [&first, &second] {
        node.withdraw().await;
        assert!(db::cluster::record_node_heartbeat(
            pool,
            &node.endpoint().unwrap().id().to_string(),
            "test",
            &serde_json::to_string(&cluster::NodeAddress::of(&node.endpoint().unwrap().addr()))
                .unwrap(),
            Some(100),
        )
        .await
        .unwrap());
    }
    assert!(db::cluster::set_replicas(pool, "2").await.unwrap());
    pass(&primary).await;
    pass(&first).await;
    pass(&second).await;
    assert!(
        has_copy(&first) && !has_copy(&second),
        "2: one copy, on the older node"
    );

    second.shutdown().await;
    first.shutdown().await;
    primary.shutdown().await;
}

#[tokio::test]
async fn the_loop_copies_on_its_own_and_stops_at_shutdown() {
    let database = native::PostgresDatabase::new().await;
    let root = tempfile::tempdir().unwrap();
    let (primary, copy) = two_storage_nodes(&database.pool, root.path()).await;
    let repo = create_repository(&database.pool, &primary, "looped").await;
    let uuid = rpc::repository_uuid(&repo.repository.disk_path).unwrap();
    repo.save("main", "a.txt", "unborn", b"a").await;

    let (stop, stop_signal) = watch::channel(false);
    let primary_loop = tokio::spawn(replicas::run(primary.context.clone(), stop_signal.clone()));
    let copy_loop = tokio::spawn(replicas::run(copy.context.clone(), stop_signal));
    tokio::time::timeout(Duration::from_secs(90), async {
        loop {
            let version = db::cluster::get_write_version(repo.pool, repo.id())
                .await
                .unwrap()
                .unwrap();
            if applied_on(repo.pool, repo.id(), &copy).await == Some(version) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .expect("the copy catches up without any manual pass");
    let copied = RepoStore::from_disk_path(copy_path(&copy, uuid).to_str().unwrap());
    let original = RepoStore::from_disk_path(primary_path(&primary, uuid).to_str().unwrap());
    assert_eq!(copied.refs().await.unwrap(), original.refs().await.unwrap());

    stop.send_replace(true);
    for task in [primary_loop, copy_loop] {
        tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .expect("the loop stops at shutdown")
            .unwrap();
    }
    copy.shutdown().await;
    primary.shutdown().await;
}

#[tokio::test]
async fn promotion_waits_until_copy_maintenance_releases_the_directory() {
    const CHILD_ROOT: &str = "REPLICA_MAINTENANCE_TEST_ROOT";
    let root = match std::env::var_os(CHILD_ROOT) {
        Some(root) => PathBuf::from(root),
        None => {
            // Isolate PATH: the server resolves git once and other tests run concurrently.
            use std::os::unix::fs::PermissionsExt;
            let root = tempfile::tempdir().unwrap();
            let path = std::env::var_os("PATH").unwrap();
            let executable = |name: &str| {
                std::env::split_paths(&path)
                    .map(|directory| directory.join(name))
                    .find(|candidate| candidate.is_absolute() && candidate.is_file())
                    .unwrap()
            };
            let wrapper = root.path().join("git");
            std::fs::write(&wrapper, format!(
                "#!/bin/sh\ncase \" $* \" in *' gc --auto '*)\n  : > '{root}/packing'\n  while test ! -e '{root}/release'; do '{sleep}' 0.01; done\n;; esac\nexec '{git}' \"$@\"\n",
                root = root.path().display(), sleep = executable("sleep").display(),
                git = executable("git").display(),
            )).unwrap();
            std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
            let paths =
                std::iter::once(root.path().to_path_buf()).chain(std::env::split_paths(&path));
            let output = tokio::time::timeout(
                Duration::from_secs(90),
                tokio::process::Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "promotion_waits_until_copy_maintenance_releases_the_directory",
                        "--nocapture",
                    ])
                    .env(CHILD_ROOT, root.path())
                    .env("PATH", std::env::join_paths(paths).unwrap())
                    .kill_on_drop(true)
                    .output(),
            )
            .await
            .expect("maintenance test child finishes")
            .unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
    };
    let database = native::PostgresDatabase::new().await;
    let pool = &database.pool;
    let (primary, copy) = two_storage_nodes(pool, &root).await;
    let web = web_node(pool, &root).await;
    let repo = create_repository(pool, &primary, "packing").await;
    let uuid = rpc::repository_uuid(&repo.repository.disk_path).unwrap();
    let head = repo.save("main", "README.md", "unborn", b"readable").await;
    pass(&primary).await;
    let mirror = copy_path(&copy, uuid);
    git2::Repository::init_bare(&mirror).unwrap();
    // Extra pack files trigger the maintenance count without changing the copy's refs.
    for index in 0..51 {
        std::fs::write(mirror.join(format!("objects/pack/extra-{index}.pack")), b"").unwrap();
    }
    let (stop, signal) = watch::channel(false);
    let copying = tokio::spawn(replicas::run(copy.context.clone(), signal));
    tokio::time::timeout(Duration::from_secs(30), async {
        while !root.join("packing").exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("copy reaches maintenance");
    assert_eq!(
        applied_on(pool, repo.id(), &copy).await,
        Some(-1),
        "maintenance must finish before readiness is published"
    );
    let command = Command::Promote {
        repository: Some("packing_owner/packing".into()),
        to: Some("store-b".into()),
        all_from: None,
        force: false,
    };
    assert!(cluster_cli::run(&command, pool, &root, &root)
        .await
        .is_err());
    let (reply, body) = web
        .context
        .call(
            copy.context.node_id,
            request(
                uuid,
                Operation::Blob {
                    git_ref: "main".into(),
                    path: "README.md".into(),
                },
            ),
            b"",
        )
        .await
        .unwrap();
    assert!(reply.result.is_ok());
    assert_eq!(body, b"readable");
    std::fs::write(root.join("release"), b"").unwrap();
    tokio::time::timeout(Duration::from_secs(30), async {
        while applied_on(pool, repo.id(), &copy).await != Some(1) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("maintenance finishes and publishes readiness");
    cluster_cli::run(&command, pool, &root, &root)
        .await
        .unwrap();
    stop.send_replace(true);
    copying.await.unwrap();
    pass(&copy).await;
    let promoted = db::repositories::records::get_repository_by_id(pool, repo.id())
        .await
        .unwrap()
        .unwrap();
    let store = repo_store(&promoted, Some(&copy.context));
    assert_eq!(store.refs().await.unwrap()["refs/heads/main"], head);
    store
        .create_branch_as(pool, repo.id(), repo.actor, "after", &head)
        .await
        .unwrap();
    web.shutdown().await;
    copy.shutdown().await;
    primary.shutdown().await;
}

#[tokio::test]
async fn completed_copies_refill_slots_before_the_next_periodic_pass() {
    let database = native::PostgresDatabase::new().await;
    let pool = &database.pool;
    let root = tempfile::tempdir().unwrap();
    let (primary, copy) = two_storage_nodes(pool, root.path()).await;
    let mut repositories = Vec::new();
    for index in 0..8 {
        repositories.push(
            create_repository(pool, &primary, &format!("backlog{index}"))
                .await
                .id(),
        );
    }
    pass(&primary).await;
    let (stop, signal) = watch::channel(false);
    let copying = tokio::spawn(replicas::run(copy.context.clone(), signal));
    let caught_up = tokio::time::timeout(Duration::from_secs(9), async {
        loop {
            let held = db::cluster::list_held_replicas(pool, copy.context.node_id)
                .await
                .unwrap();
            if held.len() == repositories.len()
                && held
                    .iter()
                    .all(|row| row.applied_version == row.write_version)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    stop.send_replace(true);
    copying.await.unwrap();
    assert!(
        caught_up.is_ok(),
        "eight fast copies must not need four periodic passes"
    );
    copy.shutdown().await;
    primary.shutdown().await;
}

/// A pass starts copies after the repository whose copy started last. So two repositories
/// that are behind again before every pass cannot keep a third from its copy.
#[tokio::test]
async fn busy_repositories_do_not_starve_later_copies() {
    let database = native::PostgresDatabase::new().await;
    let pool = &database.pool;
    let root = tempfile::tempdir().unwrap();
    let (primary, copy) = two_storage_nodes(pool, root.path()).await;
    let mut repositories = Vec::new();
    for name in ["first", "second", "third"] {
        let repo = create_repository(pool, &primary, name).await;
        repo.save("main", "a.txt", "unborn", b"a").await;
        repositories.push(repo.id());
    }
    pass(&primary).await;

    let mut retry = Retry::default();
    let mut silent = Silent::default();
    let mut copying = Copying::default();
    for _ in 0..3 {
        for &busy in &repositories[..2] {
            db::cluster::bump_write_version(pool, busy).await.unwrap();
        }
        replicas::pass(&copy.context, &mut retry, &mut silent, &mut copying)
            .await
            .unwrap();
    }
    let quiet = repositories[2];
    let version = db::cluster::get_write_version(pool, quiet)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        applied_on(pool, quiet, &copy).await,
        Some(version),
        "the third repository has its copy within three passes"
    );

    copy.shutdown().await;
    primary.shutdown().await;
}

/// Promotion refuses a copy that is behind, picks the copy with the most writes, and a
/// forced promotion records the writes the new primary lacks.
async fn check_promotion(pool: &DbPool) {
    db::node_config::upsert_node_configuration(pool, "public", true)
        .await
        .unwrap();
    let a = storage_node(pool, "promo-a").await;
    let b = reporting_storage_node(pool, "promo-b").await;
    let c = reporting_storage_node(pool, "promo-c").await;
    let repo = placed_repository(pool, a).await;
    db::cluster::bump_write_version(pool, repo).await.unwrap();
    db::cluster::bump_write_version(pool, repo).await.unwrap();
    assert!(db::cluster::insert_replica(pool, repo, b, a).await.unwrap());
    assert!(db::cluster::insert_replica(pool, repo, c, a).await.unwrap());
    assert!(db::cluster::set_applied_version(pool, repo, b, a, 2)
        .await
        .unwrap());

    assert!(
        replicas::promote(pool, repo, Some(c), false).await.is_err(),
        "a copy that is behind is refused"
    );
    assert_eq!(primary_of(pool, repo).await, a, "a refusal changes nothing");

    let promoted = replicas::promote(pool, repo, None, false).await.unwrap();
    assert_eq!(
        promoted,
        vec![Promotion {
            repository: repo,
            from: a,
            to: b,
            lost_writes: 0
        }],
        "the copy with the most writes is chosen"
    );
    assert_eq!(primary_of(pool, repo).await, b);

    assert!(
        replicas::promote(pool, repo, Some(c), true).await.is_err(),
        "a copy that was never made cannot be promoted, even forced"
    );
    assert_eq!(primary_of(pool, repo).await, b);
    assert!(db::cluster::set_applied_version(pool, repo, c, b, 1)
        .await
        .unwrap());
    assert!(
        !db::cluster::set_applied_version(pool, repo, c, a, 2)
            .await
            .unwrap(),
        "a copy taken from the old primary is not recorded after the promotion"
    );
    let forced = replicas::promote(pool, repo, Some(c), true).await.unwrap();
    assert_eq!(
        forced,
        vec![Promotion {
            repository: repo,
            from: b,
            to: c,
            lost_writes: 1
        }],
        "a forced promotion records the writes that are lost"
    );
    assert_eq!(primary_of(pool, repo).await, c);

    // A copy that was ahead of the promoted one is no longer current and copies again.
    let d = storage_node(pool, "promo-d").await;
    let ahead = placed_repository(pool, d).await;
    for _ in 0..3 {
        db::cluster::bump_write_version(pool, ahead).await.unwrap();
    }
    assert!(db::cluster::insert_replica(pool, ahead, b, d)
        .await
        .unwrap());
    assert!(db::cluster::insert_replica(pool, ahead, c, d)
        .await
        .unwrap());
    assert!(db::cluster::set_applied_version(pool, ahead, b, d, 3)
        .await
        .unwrap());
    assert!(db::cluster::set_applied_version(pool, ahead, c, d, 1)
        .await
        .unwrap());
    let behind = replicas::promote(pool, ahead, Some(c), true).await.unwrap();
    assert_eq!(
        behind,
        vec![Promotion {
            repository: ahead,
            from: d,
            to: c,
            lost_writes: 2
        }]
    );
    assert_eq!(
        db::cluster::list_replicas(pool, ahead).await.unwrap(),
        vec![(b, 1), (d, -1)],
        "the copy that was ahead is capped at the promoted copy, the old primary starts over"
    );
}

async fn primary_of(pool: &DbPool, repository: i64) -> i64 {
    db::repositories::records::get_repository_by_id(pool, repository)
        .await
        .unwrap()
        .unwrap()
        .storage_node_id
        .unwrap()
}

#[tokio::test]
async fn promotion_on_sqlite() {
    let database = native::NativeDatabase::new().await;
    check_promotion(&database.pool).await;
}

#[tokio::test]
async fn promotion_on_postgres() {
    let database = native::PostgresDatabase::new().await;
    check_promotion(&database.pool).await;
}

/// The delete returns the row it deleted. Cleanup removes files on that row's primary, so a
/// promotion after the caller's read must not send cleanup to the old primary.
async fn check_delete_names_the_promoted_primary(pool: &DbPool) {
    db::node_config::upsert_node_configuration(pool, "public", true)
        .await
        .unwrap();
    let a = storage_node(pool, "delete-a").await;
    let b = reporting_storage_node(pool, "delete-b").await;
    let repo = placed_repository(pool, a).await;
    assert!(db::cluster::insert_replica(pool, repo, b, a).await.unwrap());
    let before = db::repositories::records::get_repository_by_id(pool, repo)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(before.storage_node_id, Some(a));
    // Only a copy that has been made can take over, even when forced.
    assert!(db::cluster::set_applied_version(pool, repo, b, a, 0)
        .await
        .unwrap());

    let promoted = db::cluster::promote_repository(pool, repo, a, b, false)
        .await
        .unwrap();
    assert!(matches!(promoted, PromoteOutcome::Promoted { .. }));

    let owner = before.owner_user_id.unwrap();
    let deleted = db::repositories::records::delete_repository(pool, owner, repo)
        .await
        .unwrap();
    assert_eq!(deleted.storage_node_id, Some(b));
    assert_eq!(deleted.disk_path, before.disk_path);
}

#[tokio::test]
async fn delete_names_the_promoted_primary_on_sqlite() {
    let database = native::NativeDatabase::new().await;
    check_delete_names_the_promoted_primary(&database.pool).await;
}

#[tokio::test]
async fn delete_names_the_promoted_primary_on_postgres() {
    let database = native::PostgresDatabase::new().await;
    check_delete_names_the_promoted_primary(&database.pool).await;
}

#[tokio::test]
async fn a_promoted_copy_takes_writes_and_the_old_primary_follows() {
    let database = native::PostgresDatabase::new().await;
    let root = tempfile::tempdir().unwrap();
    let (primary, copy) = two_storage_nodes(&database.pool, root.path()).await;
    let repo = create_repository(&database.pool, &primary, "moved").await;
    let uuid = rpc::repository_uuid(&repo.repository.disk_path).unwrap();
    repo.save("main", "a.txt", "unborn", b"a").await;
    assert_current(&primary, &copy, &repo).await;

    let promotion = replicas::promote(&database.pool, repo.id(), None, false)
        .await
        .unwrap();
    assert_eq!(
        promotion,
        vec![Promotion {
            repository: repo.id(),
            from: primary.context.node_id,
            to: copy.context.node_id,
            lost_writes: 0
        }]
    );
    // The copy's directory moves to the primary location on its next pass.
    pass(&copy).await;
    assert!(primary_path(&copy, uuid).is_dir());
    assert!(!copy_path(&copy, uuid).exists());

    // Writes now go to the new primary, which is the database's node for the repository.
    let promoted = db::repositories::records::get_repository_by_id(&database.pool, repo.id())
        .await
        .unwrap()
        .unwrap();
    let writes = repo_store(&promoted, Some(&copy.context));
    let head = writes.refs().await.unwrap()["refs/heads/main"].clone();
    writes
        .create_branch_as(&database.pool, repo.id(), repo.actor, "after", &head)
        .await
        .unwrap();

    // The old primary keeps a copy with nothing applied, and follows the new primary.
    assert_eq!(
        applied_on(&database.pool, repo.id(), &primary).await,
        Some(-1)
    );
    assert_current(&copy, &primary, &repo).await;
    assert!(copy_path(&primary, uuid).is_dir());
    assert!(!primary_path(&primary, uuid).exists());

    copy.shutdown().await;
    primary.shutdown().await;
}

/// Cluster nodes need PostgreSQL, so this runs only there.
#[tokio::test]
async fn returning_former_primary_cleanup_on_postgres() {
    let database = native::PostgresDatabase::new().await;
    let pool = &database.pool;
    let root = tempfile::tempdir().unwrap();
    let (former, primary) = two_storage_nodes(pool, root.path()).await;
    let repo = create_repository(pool, &former, "retired").await;
    let deleted = create_repository(pool, &former, "deleted").await;
    let live = create_repository(pool, &former, "live").await;
    let unplaced_store = RepoStore::create(&former.context.repos_path, "main", None)
        .await
        .unwrap();
    let unplaced = db::repositories::records::create_repository(
        pool,
        repo.actor,
        "unplaced",
        None,
        false,
        unplaced_store.location(),
    )
    .await
    .unwrap();
    let unrelated = former.context.repos_path.join("not-a-uuid.git");
    std::fs::create_dir_all(&unrelated).unwrap();
    pass(&former).await;
    pass(&primary).await;
    former.withdraw().await;
    // Two slots per manual pass; include the third repository before moving anything.
    pass(&primary).await;
    // Started only now: while it is online, a new copy goes to whichever node reports more
    // free space, and the promotions below need every copy on `primary`.
    let replacement = start_storage(pool, root.path(), "store-c").await;
    for row in [&repo, &deleted] {
        replicas::promote(pool, row.id(), Some(primary.context.node_id), false)
            .await
            .unwrap();
    }
    pass(&primary).await;
    assert!(
        applied_on(pool, repo.id(), &former).await.is_some(),
        "offline copies stay until replacements are ready"
    );
    pass(&replacement).await;
    pass(&primary).await;
    assert_eq!(applied_on(pool, repo.id(), &former).await, None);
    assert_eq!(applied_on(pool, deleted.id(), &former).await, None);
    db::repositories::records::delete_repository(pool, deleted.actor, deleted.id())
        .await
        .unwrap();

    former.publish().await.unwrap();
    pass(&former).await;
    for row in [&repo, &deleted] {
        let uuid = rpc::repository_uuid(&row.repository.disk_path).unwrap();
        assert!(
            !primary_path(&former, uuid).exists(),
            "an unassigned former primary must be retired"
        );
        assert!(!copy_path(&former, uuid).exists());
    }
    assert!(primary_path(
        &former,
        rpc::repository_uuid(&live.repository.disk_path).unwrap()
    )
    .is_dir());
    assert!(
        Path::new(&unplaced.disk_path).is_dir(),
        "unplaced pre-cluster repositories survive"
    );
    assert!(unrelated.is_dir());
    replacement.shutdown().await;
    primary.shutdown().await;
    former.shutdown().await;
}

#[tokio::test]
async fn a_former_primary_with_a_copy_keeps_the_repository_through_cleanup() {
    let database = native::PostgresDatabase::new().await;
    let pool = &database.pool;
    let root = tempfile::tempdir().unwrap();
    let (former, primary) = two_storage_nodes(pool, root.path()).await;
    let repo = create_repository(pool, &former, "kept").await;
    let uuid = rpc::repository_uuid(&repo.repository.disk_path).unwrap();
    repo.save("main", "README.md", "unborn", b"kept\n").await;
    assert_current(&former, &primary, &repo).await;
    replicas::promote(pool, repo.id(), Some(primary.context.node_id), false)
        .await
        .unwrap();
    pass(&primary).await;
    // The former primary's directory is still there, and it now holds a copy row.
    assert!(primary_path(&former, uuid).is_dir());
    pass(&former).await;
    assert!(
        copy_path(&former, uuid).is_dir(),
        "the former primary keeps its copy through cleanup"
    );
    assert_current(&primary, &former, &repo).await;
    primary.shutdown().await;
    former.shutdown().await;
}

#[tokio::test]
async fn a_former_primary_without_a_copy_row_drops_its_directory() {
    let database = native::PostgresDatabase::new().await;
    let pool = &database.pool;
    let root = tempfile::tempdir().unwrap();
    let (former, primary) = two_storage_nodes(pool, root.path()).await;
    let repo = create_repository(pool, &former, "dropped").await;
    let uuid = rpc::repository_uuid(&repo.repository.disk_path).unwrap();
    repo.save("main", "README.md", "unborn", b"dropped\n").await;
    assert_current(&former, &primary, &repo).await;
    replicas::promote(pool, repo.id(), Some(primary.context.node_id), false)
        .await
        .unwrap();
    // The new primary retires the former copy row once a replacement is ready.
    assert!(db::cluster::delete_replica(
        pool,
        repo.id(),
        former.context.node_id,
        primary.context.node_id,
    )
    .await
    .unwrap());
    pass(&former).await;
    assert!(
        !primary_path(&former, uuid).exists(),
        "a former primary without a copy row removes its directory"
    );
    assert!(!copy_path(&former, uuid).exists());
    primary.shutdown().await;
    former.shutdown().await;
}

/// A storage node registered as active, with its key so the test can make it report in.
async fn keyed_storage_node(pool: &DbPool, name: &str) -> (i64, String) {
    let key = NodeKey::generate().public_hex();
    let node = db::cluster::register_own_node(pool, &key, name, &["storage".to_owned()], "test")
        .await
        .unwrap();
    (node.id, key)
}

async fn report_in(pool: &DbPool, key: &str) {
    assert!(
        db::cluster::record_node_heartbeat(pool, key, "test", "{}", None)
            .await
            .unwrap()
    );
}

/// `node` holds a copy of `repo` that has applied `applied` writes.
async fn add_copy(pool: &DbPool, repo: i64, node: i64, primary: i64, applied: i64) {
    let added = db::cluster::insert_replica(pool, repo, node, primary).await;
    assert!(added.unwrap());
    let set = db::cluster::set_applied_version(pool, repo, node, primary, applied).await;
    set.unwrap();
}

/// A repository on `primary`, forked from `source` when one is given. Each one has an owner
/// of its own, so the names only need to be unique within one test.
async fn family_member(pool: &DbPool, name: &str, primary: i64, source: Option<i64>) -> i64 {
    let owner = db::users::accounts::create_user(pool, &format!("{name}_owner"), None)
        .await
        .unwrap();
    let disk_path = format!("/srv/repos/{}.git", Uuid::new_v4());
    db::repositories::creation::insert_repository(
        pool,
        db::repositories::creation::NewRepository {
            owner: db::repositories::creation::RepositoryCreationOwner::personal(owner.id),
            name,
            description: None,
            is_private: false,
            disk_path: &disk_path,
            storage_node_id: Some(primary),
            source_id: source,
        },
    )
    .await
    .unwrap()
    .unwrap()
    .id
}

/// Puts a repository on another primary than its source's, as promotion of one fork did
/// before families moved together.
async fn move_primary(pool: &DbPool, repository: i64, node: i64) {
    let changed = match pool {
        DbPool::Postgres(raw) => {
            sqlx::query("UPDATE repositories SET storage_node_id = $1 WHERE id = $2")
                .bind(node)
                .bind(repository)
                .execute(raw)
                .await
                .unwrap()
                .rows_affected()
        }
        DbPool::Sqlite(raw) => {
            sqlx::query("UPDATE repositories SET storage_node_id = ?1 WHERE id = ?2")
                .bind(node)
                .bind(repository)
                .execute(raw)
                .await
                .unwrap()
                .rows_affected()
        }
    };
    assert_eq!(changed, 1);
}

async fn take_over(pool: &DbPool, node: i64, silent: &mut Silent, wait: Duration) -> Vec<i64> {
    // The metadata fixtures need usable directories as well as recorded versions.
    let root = tempfile::tempdir().unwrap();
    for row in db::cluster::list_held_replicas(pool, node).await.unwrap() {
        if row.applied_version >= 0 {
            let uuid = rpc::repository_uuid(&row.disk_path).unwrap();
            git2::Repository::init_bare(root.path().join("mirrors").join(format!("{uuid}.git")))
                .unwrap();
        }
    }
    replicas::take_over(pool, node, root.path(), silent, wait)
        .await
        .unwrap()
}

/// A copy takes over only from a primary this node has seen offline for the whole wait, and
/// only when it holds every write.
async fn check_take_over(pool: &DbPool) {
    let (a, _) = keyed_storage_node(pool, "store-a").await;
    let (b, b_key) = keyed_storage_node(pool, "store-b").await;
    let (c, c_key) = keyed_storage_node(pool, "store-c").await;
    let (revoked, _) = keyed_storage_node(pool, "store-revoked").await;
    report_in(pool, &b_key).await;
    report_in(pool, &c_key).await;
    let (hour, now) = (Duration::from_secs(3600), Duration::ZERO);

    // `a` has never reported in, so it is offline. `b` holds every write and `c` does not.
    let repo = placed_repository(pool, a).await;
    for _ in 0..2 {
        db::cluster::bump_write_version(pool, repo).await.unwrap();
    }
    add_copy(pool, repo, b, a, 2).await;
    add_copy(pool, repo, c, a, 1).await;
    let early = take_over(pool, b, &mut Silent::default(), hour).await;
    assert!(early.is_empty());
    let behind = take_over(pool, c, &mut Silent::default(), now).await;
    assert!(behind.is_empty(), "a copy that is behind waits");
    assert_eq!(primary_of(pool, repo).await, a);

    // A primary that was revoked is left to the operator.
    let kept = placed_repository(pool, revoked).await;
    db::cluster::bump_write_version(pool, kept).await.unwrap();
    add_copy(pool, kept, b, revoked, 1).await;
    cluster_revoke(pool, "store-revoked").await;

    let taken = take_over(pool, b, &mut Silent::default(), now).await;
    assert_eq!(taken, vec![repo]);
    assert_eq!(primary_of(pool, repo).await, b);
    assert_eq!(primary_of(pool, kept).await, revoked);
    let again = take_over(pool, b, &mut Silent::default(), now).await;
    assert!(again.is_empty(), "nothing is left to take");

    // The wait counts from when the node saw the primary go offline, and starts again if
    // the primary comes back.
    let (e, e_key) = keyed_storage_node(pool, "store-e").await;
    let flaky = placed_repository(pool, e).await;
    add_copy(pool, flaky, b, e, 0).await;
    let (wait, mut silent) = (Duration::from_millis(100), Silent::default());
    report_in(pool, &e_key).await;
    assert!(take_over(pool, b, &mut silent, wait).await.is_empty());
    clear_heartbeat(pool, &e_key).await;
    assert!(take_over(pool, b, &mut silent, wait).await.is_empty());
    tokio::time::sleep(wait).await;
    report_in(pool, &e_key).await;
    assert!(take_over(pool, b, &mut silent, wait).await.is_empty());
    clear_heartbeat(pool, &e_key).await;
    let restarted = take_over(pool, b, &mut silent, wait).await;
    assert!(restarted.is_empty(), "coming back restarted the wait");
    tokio::time::sleep(wait).await;
    assert_eq!(take_over(pool, b, &mut silent, wait).await, vec![flaky]);
}

async fn clear_heartbeat(pool: &DbPool, key: &str) {
    db::cluster::clear_node_heartbeat(pool, key).await.unwrap();
}

async fn cluster_revoke(pool: &DbPool, name: &str) {
    cluster::revoke(pool, db::cluster::Authority::Operator, name)
        .await
        .unwrap();
}

async fn check_heartbeat_clock(pool: &DbPool) {
    let (a, key) = keyed_storage_node(pool, "clock-a").await;
    let b = reporting_storage_node(pool, "clock-b").await;
    let repo = family_member(pool, "clock", a, None).await;
    add_copy(pool, repo, b, a, 0).await;
    report_in(pool, &key).await;
    let node = db::cluster::get_node_by_id(pool, a).await.unwrap().unwrap();
    assert!(node.is_online());
    // Move the database clock and its heartbeat together, without moving this process's
    // clock. Both ahead and behind must classify the same age in the same way.
    for hours in [-2, 2] {
        let mut shifted = node.clone();
        shifted.observed_at += chrono::Duration::hours(hours);
        shifted.last_seen = node
            .last_seen
            .map(|seen| seen + chrono::Duration::hours(hours));
        assert!(shifted.is_online());
        shifted.last_seen = Some(shifted.observed_at - chrono::Duration::seconds(61));
        assert!(!shifted.is_online());
        shifted.last_seen = Some(shifted.observed_at + chrono::Duration::seconds(1));
        assert!(!shifted.is_online(), "future heartbeats are not fresh");
    }
    // A primary that returned after the caller's offline observation must not move.
    assert_eq!(
        db::cluster::promote_fork_family(pool, repo, a, b)
            .await
            .unwrap(),
        FamilyOutcome::Refused
    );
    assert_eq!(primary_of(pool, repo).await, a);
    clear_heartbeat(pool, &key).await;
    assert!(!db::cluster::get_node_by_id(pool, a)
        .await
        .unwrap()
        .unwrap()
        .is_online());
    assert_eq!(
        db::cluster::promote_fork_family(pool, repo, a, b)
            .await
            .unwrap(),
        FamilyOutcome::Promoted(vec![repo])
    );
}

#[tokio::test]
async fn heartbeat_clock_on_sqlite() {
    let database = native::NativeDatabase::new().await;
    check_heartbeat_clock(&database.pool).await;
}

#[tokio::test]
async fn heartbeat_clock_on_postgres() {
    let database = native::PostgresDatabase::new().await;
    check_heartbeat_clock(&database.pool).await;
}

#[tokio::test]
async fn take_over_on_sqlite() {
    let database = native::NativeDatabase::new().await;
    check_take_over(&database.pool).await;
}

#[tokio::test]
async fn take_over_on_postgres() {
    let database = native::PostgresDatabase::new().await;
    check_take_over(&database.pool).await;
}

async fn check_missing_copy_blocks_take_over(pool: &DbPool) {
    let a = storage_node(pool, "lost-a").await;
    let b = reporting_storage_node(pool, "lost-b").await;
    let source = family_member(pool, "lostsrc", a, None).await;
    let fork = family_member(pool, "lostfork", a, Some(source)).await;
    let root = tempfile::tempdir().unwrap();
    for repo in [source, fork] {
        add_copy(pool, repo, b, a, 0).await;
    }
    let rows = db::cluster::list_held_replicas(pool, b).await.unwrap();
    let paths: Vec<_> = rows
        .iter()
        .map(|row| {
            let uuid = rpc::repository_uuid(&row.disk_path).unwrap();
            root.path().join("mirrors").join(format!("{uuid}.git"))
        })
        .collect();
    for path in &paths {
        git2::Repository::init_bare(path).unwrap();
    }
    std::fs::remove_dir_all(&paths[1]).unwrap();
    let mut silent = Silent::default();
    assert!(
        replicas::take_over(pool, b, root.path(), &mut silent, Duration::ZERO)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(primary_of(pool, source).await, a);
    assert_eq!(primary_of(pool, fork).await, a);
    assert_eq!(
        db::cluster::list_replicas(pool, fork).await.unwrap(),
        vec![(b, -1)]
    );
    assert_eq!(
        db::cluster::promote_fork_family(pool, source, a, b)
            .await
            .unwrap(),
        FamilyOutcome::Refused
    );
    assert!(matches!(
        db::cluster::promote_repository(pool, fork, a, b, false)
            .await
            .unwrap(),
        PromoteOutcome::Stale {
            applied_version: -1,
            ..
        }
    ));

    git2::Repository::init_bare(&paths[1]).unwrap();
    assert!(
        replicas::take_over(pool, b, root.path(), &mut silent, Duration::ZERO)
            .await
            .unwrap()
            .is_empty(),
        "an initialized directory is not a completed copy"
    );
    db::cluster::set_applied_version(pool, fork, b, a, 0)
        .await
        .unwrap();
    assert_eq!(
        replicas::take_over(pool, b, root.path(), &mut silent, Duration::ZERO)
            .await
            .unwrap(),
        vec![source, fork]
    );
}

#[tokio::test]
async fn missing_copy_blocks_take_over_on_sqlite() {
    let database = native::NativeDatabase::new().await;
    check_missing_copy_blocks_take_over(&database.pool).await;
}

#[tokio::test]
async fn missing_copy_blocks_take_over_on_postgres() {
    let database = native::PostgresDatabase::new().await;
    check_missing_copy_blocks_take_over(&database.pool).await;
}

#[tokio::test]
async fn lost_current_copies_are_rebuilt_without_a_primary_write() {
    let database = native::PostgresDatabase::new().await;
    let pool = &database.pool;
    let root = tempfile::tempdir().unwrap();
    let (primary, copy) = two_storage_nodes(pool, root.path()).await;
    let repo = create_repository(pool, &primary, "rebuild").await;
    let uuid = rpc::repository_uuid(&repo.repository.disk_path).unwrap();
    let head = repo.save("main", "README.md", "unborn", b"survives").await;
    assert_current(&primary, &copy, &repo).await;
    let version = db::cluster::get_write_version(pool, repo.id())
        .await
        .unwrap();
    let copy_id = copy.context.node_id;
    copy.shutdown().await;
    std::fs::remove_dir_all(&copy.context.repos_path).unwrap();
    let copy = start_storage(pool, root.path(), "store-b").await;
    assert_eq!(copy.context.node_id, copy_id);
    assert_eq!(applied_on(pool, repo.id(), &copy).await, Some(-1));
    let command = Command::Promote {
        repository: Some("rebuild_owner/rebuild".into()),
        to: Some("store-b".into()),
        all_from: None,
        force: true,
    };
    assert!(cluster_cli::run(&command, pool, root.path(), root.path())
        .await
        .is_err());
    pass(&copy).await;
    assert_eq!(applied_on(pool, repo.id(), &copy).await, version);
    let mirror = copy_path(&copy, uuid);
    assert_eq!(
        RepoStore::from_disk_path(mirror.to_str().unwrap())
            .refs()
            .await
            .unwrap()["refs/heads/main"],
        head
    );

    std::fs::remove_dir_all(&mirror).unwrap();
    pass(&copy).await;
    assert_eq!(applied_on(pool, repo.id(), &copy).await, version);
    assert_eq!(
        RepoStore::from_disk_path(mirror.to_str().unwrap())
            .refs()
            .await
            .unwrap()["refs/heads/main"],
        head
    );
    assert_eq!(
        db::cluster::get_write_version(pool, repo.id())
            .await
            .unwrap(),
        version
    );
    primary.shutdown().await;
    std::fs::remove_dir_all(&mirror).unwrap();
    assert!(replicas::take_over(
        pool,
        copy_id,
        &copy.context.repos_path,
        &mut Silent::default(),
        Duration::ZERO
    )
    .await
    .unwrap()
    .is_empty());
    assert_eq!(primary_of(pool, repo.id()).await, primary.context.node_id);
    copy.shutdown().await;
}

async fn check_offline_target_is_refused(pool: &DbPool) {
    let a = storage_node(pool, "offline-a").await;
    let (b, b_key) = keyed_storage_node(pool, "offline-b").await;
    let repo = family_member(pool, "offline", a, None).await;
    add_copy(pool, repo, b, a, 0).await;
    // `b` holds a current copy but is not reporting in, so it cannot take the repository.
    assert_eq!(
        db::cluster::promote_repository(pool, repo, a, b, true)
            .await
            .unwrap(),
        PromoteOutcome::TargetOffline
    );
    assert_eq!(
        db::cluster::promote_fork_family(pool, repo, a, b)
            .await
            .unwrap(),
        FamilyOutcome::Refused
    );
    assert_eq!(primary_of(pool, repo).await, a);
    report_in(pool, &b_key).await;
    assert!(matches!(
        db::cluster::promote_repository(pool, repo, a, b, true)
            .await
            .unwrap(),
        PromoteOutcome::Promoted { .. }
    ));
    assert_eq!(primary_of(pool, repo).await, b);
}

#[tokio::test]
async fn offline_target_is_refused_on_sqlite() {
    let database = native::NativeDatabase::new().await;
    check_offline_target_is_refused(&database.pool).await;
}

#[tokio::test]
async fn offline_target_is_refused_on_postgres() {
    let database = native::PostgresDatabase::new().await;
    check_offline_target_is_refused(&database.pool).await;
}

/// A copy that Git cannot open is cleared when its node publishes, and rebuilt from the
/// primary. A copy whose HEAD is gone is cleared on the next pass and rebuilt the same way.
#[tokio::test]
async fn a_copy_git_cannot_open_is_cleared_and_rebuilt() {
    let database = native::PostgresDatabase::new().await;
    let pool = &database.pool;
    let root = tempfile::tempdir().unwrap();
    let (primary, copy) = two_storage_nodes(pool, root.path()).await;
    let repo = create_repository(pool, &primary, "damaged").await;
    let uuid = rpc::repository_uuid(&repo.repository.disk_path).unwrap();
    let head = repo.save("main", "README.md", "unborn", b"survives").await;
    assert_current(&primary, &copy, &repo).await;
    let version = db::cluster::get_write_version(pool, repo.id())
        .await
        .unwrap();
    let copy_id = copy.context.node_id;
    copy.shutdown().await;
    // The directory is still there, but its config no longer parses.
    std::fs::write(copy_path(&copy, uuid).join("config"), b"[broken").unwrap();
    let copy = start_storage(pool, root.path(), "store-b").await;
    assert_eq!(copy.context.node_id, copy_id);
    assert_eq!(applied_on(pool, repo.id(), &copy).await, Some(-1));
    pass(&copy).await;
    assert_eq!(applied_on(pool, repo.id(), &copy).await, version);
    let mirror = copy_path(&copy, uuid);
    let mirror_path = mirror.to_str().unwrap();
    assert_eq!(
        RepoStore::from_disk_path(mirror_path).refs().await.unwrap()["refs/heads/main"],
        head
    );

    // Without HEAD the copy is unusable, so the next pass clears it and rebuilds it.
    std::fs::remove_file(mirror.join("HEAD")).unwrap();
    pass(&copy).await;
    assert_eq!(applied_on(pool, repo.id(), &copy).await, version);
    assert_eq!(
        RepoStore::from_disk_path(mirror_path).refs().await.unwrap()["refs/heads/main"],
        head
    );
    // Each rebuild removes the damaged directory it replaced.
    let leftovers = std::fs::read_dir(copy.context.repos_path.join("mirrors"))
        .unwrap()
        .filter(|entry| {
            entry
                .as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains(".unusable-")
        })
        .count();
    assert_eq!(leftovers, 0);
    copy.shutdown().await;
    primary.shutdown().await;
}

/// A rebuild that failed after moving the copy aside leaves that directory, and the retry finds
/// nothing to move. The next complete copy removes it and any other stale one, and leaves
/// similarly named siblings alone.
#[tokio::test]
async fn a_complete_copy_removes_stale_unusable_directories_only() {
    let database = native::PostgresDatabase::new().await;
    let pool = &database.pool;
    let root = tempfile::tempdir().unwrap();
    let (primary, copy) = two_storage_nodes(pool, root.path()).await;
    let repo = create_repository(pool, &primary, "stale").await;
    let uuid = rpc::repository_uuid(&repo.repository.disk_path).unwrap();
    let head = repo.save("main", "README.md", "unborn", b"kept").await;
    assert_current(&primary, &copy, &repo).await;
    let copy_id = copy.context.node_id;
    copy.shutdown().await;

    // The copy is damaged, and its failed rebuild moved it aside before it stopped.
    let mirror = copy_path(&copy, uuid);
    let mirrors = mirror.parent().unwrap().to_path_buf();
    let failed = mirrors.join(format!("{uuid}.git.unusable-1000"));
    std::fs::rename(&mirror, &failed).unwrap();
    std::fs::write(failed.join("HEAD"), b"").unwrap();
    let stale = mirrors.join(format!("{uuid}.git.unusable-2000"));
    std::fs::create_dir(&stale).unwrap();
    std::fs::write(stale.join("config"), b"[broken").unwrap();
    // These are not the copy's own leftovers.
    let other_suffix = mirrors.join(format!("{uuid}.git.unusable-abc"));
    let other_copy = mirrors.join("other.git.unusable-1");
    let empty_stamp = mirrors.join(format!("{uuid}.git.unusable-"));
    let regular_file = mirrors.join(format!("{uuid}.git.unusable-3000"));
    for directory in [&other_suffix, &other_copy, &empty_stamp] {
        std::fs::create_dir(directory).unwrap();
        std::fs::write(directory.join("keep"), b"x").unwrap();
    }
    std::fs::write(&regular_file, b"x").unwrap();

    let copy = start_storage(pool, root.path(), "store-b").await;
    assert_eq!(copy.context.node_id, copy_id);
    pass(&copy).await;
    assert_eq!(
        RepoStore::from_disk_path(mirror.to_str().unwrap())
            .refs()
            .await
            .unwrap()["refs/heads/main"],
        head
    );
    assert!(
        !failed.exists(),
        "the aside directory of the failed rebuild"
    );
    assert!(!stale.exists(), "the second stale directory");
    for kept in [&other_suffix, &other_copy, &empty_stamp] {
        assert!(kept.join("keep").exists(), "{} must stay", kept.display());
    }
    assert!(regular_file.is_file());
    copy.shutdown().await;
    primary.shutdown().await;
}

/// An incremental copy of a usable copy with refs also clears stale unusable directories, which
/// a rebuild interrupted after its refs were set leaves behind. A copy that fails keeps them.
#[tokio::test]
async fn an_incremental_copy_clears_stale_unusable_directories_after_it_succeeds() {
    let database = native::PostgresDatabase::new().await;
    let pool = &database.pool;
    let root = tempfile::tempdir().unwrap();
    let (primary, copy) = two_storage_nodes(pool, root.path()).await;
    let repo = create_repository(pool, &primary, "incremental").await;
    let uuid = rpc::repository_uuid(&repo.repository.disk_path).unwrap();
    let first = repo.save("main", "README.md", "unborn", b"one").await;
    assert_current(&primary, &copy, &repo).await;
    let mirrors = copy_path(&copy, uuid).parent().unwrap().to_path_buf();
    let stale = mirrors.join(format!("{uuid}.git.unusable-1000"));
    std::fs::create_dir(&stale).unwrap();
    std::fs::write(stale.join("config"), b"[broken").unwrap();
    let decoy = mirrors.join(format!("{uuid}.git.unusable-abc"));
    let other_copy = mirrors.join("other.git.unusable-1");
    for directory in [&decoy, &other_copy] {
        std::fs::create_dir(directory).unwrap();
        std::fs::write(directory.join("keep"), b"x").unwrap();
    }

    // The primary moves ahead but cannot make a pack, so the copy fails and keeps the stale one.
    let second = repo.save("main", "NEXT.md", &first, b"two").await;
    let config = primary_path(&primary, uuid).join("config");
    let good_config = std::fs::read(&config).unwrap();
    std::fs::write(&config, b"[broken").unwrap();
    pass(&copy).await;
    assert!(stale.exists(), "a failed copy keeps the stale directory");
    let version = db::cluster::get_write_version(pool, repo.id())
        .await
        .unwrap();
    assert_ne!(applied_on(pool, repo.id(), &copy).await, version);

    // The primary recovers and the incremental copy succeeds.
    std::fs::write(&config, good_config).unwrap();
    pass(&copy).await;
    assert_eq!(applied_on(pool, repo.id(), &copy).await, version);
    let mirror = copy_path(&copy, uuid);
    assert_eq!(
        RepoStore::from_disk_path(mirror.to_str().unwrap())
            .refs()
            .await
            .unwrap()["refs/heads/main"],
        second
    );
    assert!(!stale.exists(), "the stale directory");
    for kept in [&decoy, &other_copy] {
        assert!(kept.join("keep").exists(), "{} must stay", kept.display());
    }
    copy.shutdown().await;
    primary.shutdown().await;
}

async fn check_default_target_is_reporting_in(pool: &DbPool) {
    let a = storage_node(pool, "pick-a").await;
    // `b` has the lower id and an equally current copy, but it has not reported in.
    let b = storage_node(pool, "pick-b").await;
    let (c, c_key) = keyed_storage_node(pool, "pick-c").await;
    report_in(pool, &c_key).await;
    let repo = family_member(pool, "pick", a, None).await;
    add_copy(pool, repo, b, a, 0).await;
    add_copy(pool, repo, c, a, 0).await;
    assert_eq!(
        replicas::promote(pool, repo, None, false).await.unwrap(),
        vec![Promotion {
            repository: repo,
            from: a,
            to: c,
            lost_writes: 0,
        }]
    );
    assert_eq!(primary_of(pool, repo).await, c);
}

#[tokio::test]
async fn default_promotion_takes_a_holder_that_is_reporting_in_on_sqlite() {
    let database = native::NativeDatabase::new().await;
    check_default_target_is_reporting_in(&database.pool).await;
}

#[tokio::test]
async fn default_promotion_takes_a_holder_that_is_reporting_in_on_postgres() {
    let database = native::PostgresDatabase::new().await;
    check_default_target_is_reporting_in(&database.pool).await;
}

/// A current copy that Git cannot open, here a file where the mirror belongs, is never taken
/// over, and the copy that has no mirror at all is still checked.
async fn check_unreadable_copy_does_not_stop_takeover(pool: &DbPool) {
    let a = storage_node(pool, "unread-a").await;
    let b = storage_node(pool, "unread-b").await;
    let unreadable = family_member(pool, "unreadable", a, None).await;
    let missing = family_member(pool, "missing", a, None).await;
    add_copy(pool, unreadable, b, a, 0).await;
    add_copy(pool, missing, b, a, 0).await;
    let rows = db::cluster::list_held_replicas(pool, b).await.unwrap();
    let uuid = rows
        .iter()
        .find(|row| row.repository_id == unreadable)
        .map(|row| rpc::repository_uuid(&row.disk_path).unwrap())
        .unwrap();
    let root = tempfile::tempdir().unwrap();
    let mirrors = root.path().join("mirrors");
    std::fs::create_dir_all(&mirrors).unwrap();
    // A file where the mirror belongs: Git cannot read it. `missing` has no mirror at all.
    std::fs::write(mirrors.join(format!("{uuid}.git")), b"not a repository").unwrap();
    let taken = replicas::take_over(pool, b, root.path(), &mut Silent::default(), Duration::ZERO)
        .await
        .unwrap();
    assert!(taken.is_empty());
    assert_eq!(
        db::cluster::list_replicas(pool, unreadable).await.unwrap(),
        vec![(b, -1)]
    );
    assert_eq!(
        db::cluster::list_replicas(pool, missing).await.unwrap(),
        vec![(b, -1)]
    );
}

#[tokio::test]
async fn unreadable_copy_does_not_stop_takeover_on_sqlite() {
    let database = native::NativeDatabase::new().await;
    check_unreadable_copy_does_not_stop_takeover(&database.pool).await;
}

#[tokio::test]
async fn unreadable_copy_does_not_stop_takeover_on_postgres() {
    let database = native::PostgresDatabase::new().await;
    check_unreadable_copy_does_not_stop_takeover(&database.pool).await;
}

/// One copy that Git cannot open must not stop the others from being rebuilt. A file where a
/// mirror belongs is replaced by a rebuilt copy, and so is the copy with no mirror, in the same
/// pass.
#[tokio::test]
async fn an_unreadable_copy_does_not_stop_the_others_from_being_rebuilt() {
    let database = native::PostgresDatabase::new().await;
    let pool = &database.pool;
    let root = tempfile::tempdir().unwrap();
    let (primary, copy) = two_storage_nodes(pool, root.path()).await;
    let first = create_repository(pool, &primary, "unreadable").await;
    let second = create_repository(pool, &primary, "rebuilt").await;
    let first_uuid = rpc::repository_uuid(&first.repository.disk_path).unwrap();
    let second_uuid = rpc::repository_uuid(&second.repository.disk_path).unwrap();
    let first_head = first.save("main", "README.md", "unborn", b"first").await;
    let second_head = second.save("main", "README.md", "unborn", b"second").await;
    assert_current(&primary, &copy, &first).await;
    assert_current(&primary, &copy, &second).await;
    let first_applied = applied_on(pool, first.id(), &copy).await;
    let second_version = db::cluster::get_write_version(pool, second.id())
        .await
        .unwrap();
    copy.shutdown().await;
    // `first` has a file where its mirror belongs, which Git cannot read. `second` has no mirror.
    std::fs::remove_dir_all(copy_path(&copy, first_uuid)).unwrap();
    std::fs::write(copy_path(&copy, first_uuid), b"not a repository").unwrap();
    std::fs::remove_dir_all(copy_path(&copy, second_uuid)).unwrap();
    // Starting the node publishes it, and publishing checks its copies.
    let copy = start_storage(pool, root.path(), "store-b").await;
    pass(&copy).await;
    assert_eq!(applied_on(pool, first.id(), &copy).await, first_applied);
    assert_eq!(applied_on(pool, second.id(), &copy).await, second_version);
    assert_eq!(
        RepoStore::from_disk_path(copy_path(&copy, first_uuid).to_str().unwrap())
            .refs()
            .await
            .unwrap()["refs/heads/main"],
        first_head
    );
    assert_eq!(
        RepoStore::from_disk_path(copy_path(&copy, second_uuid).to_str().unwrap())
            .refs()
            .await
            .unwrap()["refs/heads/main"],
        second_head
    );
    copy.shutdown().await;
    primary.shutdown().await;
}

#[tokio::test]
async fn a_copy_takes_over_when_its_primary_stops() {
    let database = native::PostgresDatabase::new().await;
    let root = tempfile::tempdir().unwrap();
    let (primary, copy) = two_storage_nodes(&database.pool, root.path()).await;
    let repo = create_repository(&database.pool, &primary, "failover").await;
    let uuid = rpc::repository_uuid(&repo.repository.disk_path).unwrap();
    repo.save("main", "a.txt", "unborn", b"a").await;
    assert_current(&primary, &copy, &repo).await;
    let copy_id = copy.context.node_id;
    let pool = &database.pool;
    let take = || async {
        let mut silent = Silent::default();
        replicas::take_over(
            pool,
            copy_id,
            &copy.context.repos_path,
            &mut silent,
            Duration::ZERO,
        )
        .await
        .unwrap()
    };
    assert!(take().await.is_empty(), "the primary is running");
    primary.shutdown().await;
    // A pass waits the full minute before it takes anything over.
    pass(&copy).await;
    assert_eq!(
        primary_of(&database.pool, repo.id()).await,
        primary.context.node_id
    );

    assert_eq!(take().await, vec![repo.id()]);
    pass(&copy).await;
    assert!(primary_path(&copy, uuid).is_dir());
    assert!(!copy_path(&copy, uuid).exists());
    let promoted = db::repositories::records::get_repository_by_id(&database.pool, repo.id())
        .await
        .unwrap()
        .unwrap();
    let writes = repo_store(&promoted, Some(&copy.context));
    let head = writes.refs().await.unwrap()["refs/heads/main"].clone();
    writes
        .create_branch_as(&database.pool, repo.id(), repo.actor, "after", &head)
        .await
        .unwrap();
    let refs = writes.refs().await.unwrap();
    assert!(refs.contains_key("refs/heads/after"));
    copy.shutdown().await;
}

/// A fork family moves together: promoting any member moves the members on its primary, and
/// a family that the target cannot take in full is refused whole, before anything moves.
async fn check_fork_family_promotion(pool: &DbPool) {
    let a = storage_node(pool, "fam-a").await;
    let b = reporting_storage_node(pool, "fam-b").await;
    let c = storage_node(pool, "fam-c").await;
    let source = family_member(pool, "famsrc", a, None).await;
    let fork = family_member(pool, "famfork", a, Some(source)).await;
    let fork_of_fork = family_member(pool, "famforkfork", a, Some(fork)).await;
    // A fork on another primary is not part of this move. New forks start on their
    // source's primary, so this one is created there and moved.
    let elsewhere = family_member(pool, "famelse", a, Some(source)).await;
    move_primary(pool, elsewhere, c).await;
    for repo in [source, fork, fork_of_fork] {
        add_copy(pool, repo, b, a, 0).await;
    }
    let moved = |repository| Promotion {
        repository,
        from: a,
        to: b,
        lost_writes: 0,
    };
    let promoted = replicas::promote(pool, fork, Some(b), false).await;
    assert_eq!(
        promoted.unwrap(),
        vec![moved(source), moved(fork), moved(fork_of_fork)]
    );
    for repo in [source, fork, fork_of_fork] {
        assert_eq!(primary_of(pool, repo).await, b);
    }
    assert_eq!(primary_of(pool, elsewhere).await, c);
    assert_eq!(
        db::cluster::list_replicas(pool, fork).await.unwrap(),
        vec![(a, -1)],
        "the old primary keeps a copy that catches up"
    );

    // The target holds a copy of the source only. Promoting the source is refused, and nothing
    // moves, even though the source's own copy would do.
    let d = storage_node(pool, "fam-d").await;
    let e = storage_node(pool, "fam-e").await;
    let f = reporting_storage_node(pool, "fam-f").await;
    let refsource = family_member(pool, "refsrc", d, None).await;
    let reffork = family_member(pool, "reffork", d, Some(refsource)).await;
    let refforkfork = family_member(pool, "refforkfork", d, Some(reffork)).await;
    add_copy(pool, refsource, f, d, 0).await;
    for repo in [reffork, refforkfork] {
        add_copy(pool, repo, e, d, 0).await;
    }
    let error = replicas::promote(pool, refsource, Some(f), false)
        .await
        .unwrap_err();
    assert_eq!(
        format!("{error:#}"),
        "reffork_owner/reffork has no copy on that node; nothing changed"
    );
    // `--force` accepts a split family: the members that have no copy on the target stay.
    let forced = replicas::promote(pool, refsource, Some(f), true).await;
    assert_eq!(
        forced.unwrap(),
        vec![Promotion {
            repository: refsource,
            from: d,
            to: f,
            lost_writes: 0,
        }]
    );
    assert_eq!(primary_of(pool, refsource).await, f);
    for repo in [reffork, refforkfork] {
        assert_eq!(primary_of(pool, repo).await, d);
    }

    // A forced promotion still needs the requested repository's own copy to be made.
    let g = storage_node(pool, "fam-g").await;
    let h = storage_node(pool, "fam-h").await;
    let never = family_member(pool, "nevsrc", g, None).await;
    let never_fork = family_member(pool, "nevfork", g, Some(never)).await;
    add_copy(pool, never, h, g, 0).await;
    assert!(db::cluster::insert_replica(pool, never_fork, h, g)
        .await
        .unwrap());
    let error = replicas::promote(pool, never_fork, Some(h), true)
        .await
        .unwrap_err();
    assert_eq!(
        format!("{error:#}"),
        "nevfork_owner/nevfork has not been copied to that node yet; nothing changed"
    );
    assert_eq!(primary_of(pool, never).await, g);
    assert_eq!(primary_of(pool, never_fork).await, g);
}

#[tokio::test]
async fn fork_family_promotion_on_sqlite() {
    let database = native::NativeDatabase::new().await;
    check_fork_family_promotion(&database.pool).await;
}

#[tokio::test]
async fn fork_family_promotion_on_postgres() {
    let database = native::PostgresDatabase::new().await;
    check_fork_family_promotion(&database.pool).await;
}

/// A take-over moves a fork family only when every member is on the offline primary and this
/// node holds a current copy of each. A family with a member missing here, or one that is
/// already split, waits whole.
async fn check_fork_family_take_over(pool: &DbPool) {
    let (a, _) = keyed_storage_node(pool, "tf-a").await;
    let (b, b_key) = keyed_storage_node(pool, "tf-b").await;
    report_in(pool, &b_key).await;
    let source = family_member(pool, "tfsrc", a, None).await;
    let fork = family_member(pool, "tffork", a, Some(source)).await;
    // `a` never reported in, so it is offline. `b` has the source's copy but not the fork's.
    add_copy(pool, source, b, a, 0).await;
    let now = Duration::ZERO;
    // One node's passes share one `Silent`, as its loop does, so the second pass sees the
    // family that the first one skipped.
    let mut silent = Silent::default();
    let early = take_over(pool, b, &mut silent, now).await;
    assert!(
        early.is_empty(),
        "a family with a member missing here waits"
    );
    assert_eq!(primary_of(pool, source).await, a);
    assert_eq!(primary_of(pool, fork).await, a);

    add_copy(pool, fork, b, a, 0).await;
    let taken = take_over(pool, b, &mut silent, now).await;
    assert_eq!(
        taken,
        vec![source, fork],
        "a skipped family is taken once its copies are current"
    );
    assert_eq!(primary_of(pool, source).await, b);
    assert_eq!(primary_of(pool, fork).await, b);

    // A family that is already split waits. Moving the source to `b` alone, as a promotion
    // does one repository at a time, leaves the fork on the offline `a`. `c` holds the fork.
    let (c, _) = keyed_storage_node(pool, "tf-c").await;
    let split_source = family_member(pool, "tfsplitsrc", a, None).await;
    let split_fork = family_member(pool, "tfsplitfork", a, Some(split_source)).await;
    add_copy(pool, split_source, b, a, 0).await;
    add_copy(pool, split_source, c, a, 0).await;
    add_copy(pool, split_fork, c, a, 0).await;
    let moved = db::cluster::promote_repository(pool, split_source, a, b, true).await;
    assert!(matches!(moved.unwrap(), PromoteOutcome::Promoted { .. }));
    let split_taken = take_over(pool, c, &mut Silent::default(), now).await;
    assert!(split_taken.is_empty(), "a split family waits");
    assert_eq!(primary_of(pool, split_fork).await, a);
    let refused = db::cluster::promote_fork_family(pool, split_fork, a, c).await;
    assert_eq!(refused.unwrap(), FamilyOutcome::Refused);
    assert_eq!(primary_of(pool, split_source).await, b);
    assert_eq!(primary_of(pool, split_fork).await, a);
}

#[tokio::test]
async fn fork_family_take_over_on_sqlite() {
    let database = native::NativeDatabase::new().await;
    check_fork_family_take_over(&database.pool).await;
}

#[tokio::test]
async fn fork_family_take_over_on_postgres() {
    let database = native::PostgresDatabase::new().await;
    check_fork_family_take_over(&database.pool).await;
}

/// Two take-over passes for different nodes run at once over one fork family on an offline
/// primary. Whichever locks the family first takes all of it, and the other takes nothing.
async fn check_two_take_overs_keep_a_fork_family_whole(pool: &DbPool) {
    for round in 0..8 {
        let (a, _) = keyed_storage_node(pool, &format!("race{round}-a")).await;
        // `b` and `c` report in, so only `a` is offline. Otherwise the pass that runs second
        // would take the family from whichever node the first one moved it to.
        let (b, b_key) = keyed_storage_node(pool, &format!("race{round}-b")).await;
        let (c, c_key) = keyed_storage_node(pool, &format!("race{round}-c")).await;
        report_in(pool, &b_key).await;
        report_in(pool, &c_key).await;
        let source = family_member(pool, &format!("race{round}src"), a, None).await;
        let fork = family_member(pool, &format!("race{round}fork"), a, Some(source)).await;
        for repo in [source, fork] {
            add_copy(pool, repo, b, a, 0).await;
            add_copy(pool, repo, c, a, 0).await;
        }
        let (mut silent_b, mut silent_c) = (Silent::default(), Silent::default());
        let (mut from_b, from_c) = tokio::join!(
            take_over(pool, b, &mut silent_b, Duration::ZERO),
            take_over(pool, c, &mut silent_c, Duration::ZERO),
        );
        from_b.extend(from_c);
        from_b.sort_unstable();
        assert_eq!(
            from_b,
            vec![source, fork],
            "round {round}: one pass takes the family"
        );
        let primary = primary_of(pool, source).await;
        assert!(
            primary == b || primary == c,
            "round {round}: moved to node {primary}"
        );
        assert_eq!(
            primary_of(pool, fork).await,
            primary,
            "round {round}: the family was split"
        );
    }
}

#[tokio::test]
async fn two_take_overs_keep_a_fork_family_whole_on_sqlite() {
    let database = native::NativeDatabase::new().await;
    check_two_take_overs_keep_a_fork_family_whole(&database.pool).await;
}

#[tokio::test]
async fn two_take_overs_keep_a_fork_family_whole_on_postgres() {
    let database = native::PostgresDatabase::new().await;
    check_two_take_overs_keep_a_fork_family_whole(&database.pool).await;
}

/// `cluster promote --all-from` moves a fork family in one step. The repositories that moved
/// are not promoted again from their new primary.
#[tokio::test]
async fn promote_all_from_moves_a_fork_family_once() {
    let database = native::PostgresDatabase::new().await;
    let pool = &database.pool;
    let a = storage_node(pool, "all-a").await;
    let b = reporting_storage_node(pool, "all-b").await;
    let source = family_member(pool, "allsrc", a, None).await;
    let fork = family_member(pool, "allfork", a, Some(source)).await;
    add_copy(pool, source, b, a, 0).await;
    add_copy(pool, fork, b, a, 0).await;
    let state = tempfile::tempdir().unwrap();
    let command = Command::Promote {
        repository: None,
        to: None,
        all_from: Some("all-a".into()),
        force: false,
    };
    cluster_cli::run(&command, pool, state.path(), state.path())
        .await
        .unwrap();
    assert_eq!(primary_of(pool, source).await, b);
    assert_eq!(primary_of(pool, fork).await, b);
}

/// A node that serves only web requests. It asks the storage nodes for repository data.
async fn web_node(pool: &DbPool, root: &Path) -> cluster::Cluster {
    let node = cluster::start(
        pool,
        &root.join("web-state"),
        &root.join("web-repos"),
        &"web".parse().unwrap(),
        settings("web"),
    )
    .await
    .unwrap();
    node.publish().await.unwrap();
    node
}

fn request(repository: Uuid, operation: Operation) -> Request {
    Request {
        repository,
        operation,
        actor: None,
        ci_run: None,
    }
}

/// The input a Git client sends upload-pack to fetch one commit.
fn fetch_input(sha: &str) -> String {
    let want = format!("want {sha}\n");
    format!("{:04x}{want}00000009done\n", want.len() + 4)
}

#[tokio::test]
async fn reads_are_served_by_a_copy_while_the_primary_is_down() {
    let database = native::PostgresDatabase::new().await;
    let root = tempfile::tempdir().unwrap();
    let (primary, copy) = two_storage_nodes(&database.pool, root.path()).await;
    let web = web_node(&database.pool, root.path()).await;
    let repo = create_repository(&database.pool, &primary, "failover").await;
    let uuid = rpc::repository_uuid(&repo.repository.disk_path).unwrap();
    let head = repo
        .save("main", "README.md", "unborn", b"# failover\n")
        .await;
    assert_current(&primary, &copy, &repo).await;
    let primary_id = primary.context.node_id;
    primary.shutdown().await;

    // A file, the refs, an advertisement and a clone all come from the copy.
    let (reply, body) = web
        .context
        .call(
            primary_id,
            request(
                uuid,
                Operation::Blob {
                    git_ref: "main".into(),
                    path: "README.md".into(),
                },
            ),
            b"",
        )
        .await
        .unwrap();
    assert!(reply.result.is_ok());
    assert_eq!(body, b"# failover\n".to_vec());
    let (reply, _) = web
        .context
        .call(primary_id, request(uuid, Operation::Refs), b"")
        .await
        .unwrap();
    let refs: HashMap<String, String> = serde_json::from_value(reply.result.unwrap()).unwrap();
    assert_eq!(refs["refs/heads/main"], head);
    let (reply, advertised) = web
        .context
        .call(
            primary_id,
            request(
                uuid,
                Operation::AdvertiseRefs {
                    service: GitService::UploadPack,
                },
            ),
            b"",
        )
        .await
        .unwrap();
    assert!(reply.result.is_ok());
    assert!(String::from_utf8_lossy(&advertised).contains("refs/heads/main"));
    let input = fetch_input(&head);
    let mut pack: Vec<u8> = Vec::new();
    let (_, status) = web
        .context
        .git_session(
            primary_id,
            request(
                uuid,
                Operation::GitSession(GitSession::new(GitService::UploadPack, true)),
            ),
            GitStreams {
                input: &mut input.as_bytes(),
                output: &mut pack,
                errors: &mut tokio::io::sink(),
            },
        )
        .await
        .unwrap();
    assert!(status.success());
    assert!(pack.windows(4).any(|window| window == b"PACK"));

    web.shutdown().await;
    copy.shutdown().await;
}

#[tokio::test]
async fn a_pull_request_refresh_does_not_read_heads_from_a_lagging_copy() {
    let database = native::PostgresDatabase::new().await;
    let pool = &database.pool;
    let root = tempfile::tempdir().unwrap();
    let (primary, copy) = two_storage_nodes(pool, root.path()).await;
    let web = web_node(pool, root.path()).await;
    let repo = create_repository(pool, &primary, "refresh-head").await;
    let base = repo.save("main", "README.md", "unborn", b"# base\n").await;
    repo.branch("topic", &base).await;
    assert_current(&primary, &copy, &repo).await;
    // The primary moves `topic` on and the copy is not told. The recorded head is the new commit.
    let head = repo.save("topic", "topic.txt", &base, b"topic\n").await;
    let pull = db::pull_requests::records::create_pull_request(
        pool,
        db::pull_requests::records::NewPullRequest {
            repo_id: repo.id(),
            head_repo_id: Some(repo.id()),
            author_id: repo.actor,
            title: "Lagging copy",
            body: "",
            head_branch: "topic",
            head_sha: &head,
            base_branch: "main",
            base_sha: &base,
            is_draft: false,
        },
    )
    .await
    .unwrap();
    primary.shutdown().await;

    let ci = codeyard::backend::ci::CiRuns::new(
        pool.clone(),
        Arc::new(CiConfig::default()),
        Arc::new(tokio::sync::Notify::new()),
        Some(web.context.clone()),
        Arc::new(codeyard::backend::notifications::NotificationBroadcaster::new(pool)),
    );
    // The refs must come from the primary, which is down. The refresh fails and writes nothing.
    let refreshed = codeyard::backend::repositories::pull_requests::refresh_open_pull_request(
        pool,
        &repo.repository,
        &ci,
        repo.actor,
        pull.id,
    )
    .await;
    assert!(matches!(
        refreshed,
        Err(codeyard::backend::repositories::pull_requests::branches::RefreshError::Storage)
    ));
    let recorded = db::pull_requests::records::get_pull_request_by_id(pool, pull.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(recorded.head_sha, head);
    assert_eq!(recorded.base_sha, base);

    web.shutdown().await;
    copy.shutdown().await;
}

#[tokio::test]
async fn a_commit_a_database_write_depends_on_is_not_read_from_a_copy() {
    let database = native::PostgresDatabase::new().await;
    let pool = &database.pool;
    let root = tempfile::tempdir().unwrap();
    let (primary, copy) = two_storage_nodes(pool, root.path()).await;
    let web = web_node(pool, root.path()).await;
    let repo = create_repository(pool, &primary, "commit-head").await;
    let base = repo.save("main", "README.md", "unborn", b"# base\n").await;
    repo.branch("topic", &base).await;
    assert_current(&primary, &copy, &repo).await;
    // The primary moves `topic` on and the copy keeps the older commit.
    repo.save("topic", "topic.txt", &base, b"topic\n").await;
    primary.shutdown().await;

    let store = repo_store(&repo.repository, Some(&web.context));
    // A page read may be answered by the copy, so it sees the older commit.
    assert_eq!(store.commit("topic").await.unwrap().sha, base);
    // A commit that is written to the database must come from the primary, which is down.
    assert!(store.commit_from_primary("topic").await.is_err());

    web.shutdown().await;
    copy.shutdown().await;
}

#[tokio::test]
async fn a_read_aimed_at_the_old_primary_reaches_the_promoted_copy() {
    let database = native::PostgresDatabase::new().await;
    let root = tempfile::tempdir().unwrap();
    let (primary, copy) = two_storage_nodes(&database.pool, root.path()).await;
    let web = web_node(&database.pool, root.path()).await;
    let repo = create_repository(&database.pool, &primary, "moved-away").await;
    let uuid = rpc::repository_uuid(&repo.repository.disk_path).unwrap();
    repo.save("main", "README.md", "unborn", b"# moved\n").await;
    assert_current(&primary, &copy, &repo).await;
    let primary_id = primary.context.node_id;
    primary.shutdown().await;
    replicas::promote(&database.pool, repo.id(), None, false)
        .await
        .unwrap();
    pass(&copy).await;

    // The web node still aims at the old primary, as a request in flight during a
    // take-over does. The repository's new primary answers.
    let blob = Operation::Blob {
        git_ref: "main".into(),
        path: "README.md".into(),
    };
    let (reply, body) = web
        .context
        .call(primary_id, request(uuid, blob), b"")
        .await
        .unwrap();
    assert!(reply.result.is_ok());
    assert_eq!(body, b"# moved\n".to_vec());

    web.shutdown().await;
    copy.shutdown().await;
}

#[tokio::test]
async fn a_copy_holder_reads_its_own_copy_while_the_primary_is_down() {
    let database = native::PostgresDatabase::new().await;
    let root = tempfile::tempdir().unwrap();
    db::node_config::upsert_node_configuration(&database.pool, "public", true)
        .await
        .unwrap();
    assert!(db::cluster::set_replicas(&database.pool, "2")
        .await
        .unwrap());
    let primary = start_storage(&database.pool, root.path(), "store-a").await;
    // A node with the web role as well as storage is asked for the repository itself,
    // and it cannot dial itself.
    let copy = start_node(&database.pool, root.path(), "store-b", "web,storage").await;
    let repo = create_repository(&database.pool, &primary, "own-copy").await;
    let uuid = rpc::repository_uuid(&repo.repository.disk_path).unwrap();
    let head = repo.save("main", "README.md", "unborn", b"# own\n").await;
    assert_current(&primary, &copy, &repo).await;
    let primary_id = primary.context.node_id;
    primary.shutdown().await;

    let blob = Operation::Blob {
        git_ref: "main".into(),
        path: "README.md".into(),
    };
    let (reply, body) = copy
        .context
        .call(primary_id, request(uuid, blob), b"")
        .await
        .unwrap();
    assert!(reply.result.is_ok());
    assert_eq!(body, b"# own\n".to_vec());

    let input = fetch_input(&head);
    let mut pack: Vec<u8> = Vec::new();
    let (_, status) = copy
        .context
        .git_session(
            primary_id,
            request(
                uuid,
                Operation::GitSession(GitSession::new(GitService::UploadPack, true)),
            ),
            GitStreams {
                input: &mut input.as_bytes(),
                output: &mut pack,
                errors: &mut tokio::io::sink(),
            },
        )
        .await
        .unwrap();
    assert!(status.success());
    assert!(!pack.is_empty());

    // The dial to the dead primary failed once, so a second read does not wait for it.
    let started = std::time::Instant::now();
    let (reply, _) = copy
        .context
        .call(primary_id, request(uuid, Operation::Refs), b"")
        .await
        .unwrap();
    assert!(reply.result.is_ok());
    assert!(started.elapsed() < std::time::Duration::from_secs(2));

    copy.shutdown().await;
}

#[tokio::test]
async fn unreadable_fallbacks_are_skipped_but_an_answering_primary_is_not() {
    let database = native::PostgresDatabase::new().await;
    let pool = &database.pool;
    let root = tempfile::tempdir().unwrap();
    let (primary, first) = two_storage_nodes(pool, root.path()).await;
    let healthy = start_storage(pool, root.path(), "store-c").await;
    let web = web_node(pool, root.path()).await;
    assert!(db::cluster::set_replicas(pool, "all").await.unwrap());
    let repo = create_repository(pool, &primary, "unreadable").await;
    let router = create_router(AppState::new(
        pool.clone(),
        web.context.repos_path.clone(),
        Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
        None,
        CiConfig::default(),
        Some(web.context.clone()),
    ));
    let client = http::Client::anonymous(&router);
    let url = "/api/repos/unreadable_owner/unreadable/raw/main/README.md";
    let uuid = rpc::repository_uuid(&repo.repository.disk_path).unwrap();
    repo.save("main", "README.md", "unborn", b"healthy").await;
    assert_current(&primary, &first, &repo).await;
    pass(&healthy).await;
    // The directory exists, but libgit2 cannot open its damaged metadata.
    std::fs::write(primary_path(&primary, uuid).join("config"), b"[broken").unwrap();
    std::fs::write(copy_path(&first, uuid).join("config"), b"[broken").unwrap();
    let blob = || {
        request(
            uuid,
            Operation::Blob {
                git_ref: "main".into(),
                path: "README.md".into(),
            },
        )
    };
    let primary_id = primary.context.node_id;
    let (reply, _) = web.context.call(primary_id, blob(), b"").await.unwrap();
    assert!(matches!(reply.result,
        Err(Failure::Repository(error)) if error.kind == ErrorKind::Unavailable
            && !error.is_storage_node_unavailable()));
    assert_eq!(
        client.exchange("GET", url, "", Vec::new()).await.status,
        axum::http::StatusCode::SERVICE_UNAVAILABLE
    );
    primary.shutdown().await;
    let response = client.exchange("GET", url, "", Vec::new()).await;
    assert_eq!(response.status, axum::http::StatusCode::OK);
    assert_eq!(response.body, b"healthy");
    let (reply, body) = web.context.call(primary_id, blob(), b"").await.unwrap();
    assert!(
        reply.result.is_ok(),
        "an unusable fallback must not hide the healthy copy"
    );
    assert_eq!(body, b"healthy");
    web.shutdown().await;
    healthy.shutdown().await;
    first.shutdown().await;
}

#[tokio::test]
async fn a_moved_primary_that_answers_is_final_even_when_a_copy_could_serve() {
    let database = native::PostgresDatabase::new().await;
    let pool = &database.pool;
    let root = tempfile::tempdir().unwrap();
    let (old, new) = two_storage_nodes(pool, root.path()).await;
    let copy = start_storage(pool, root.path(), "store-c").await;
    let web = web_node(pool, root.path()).await;
    assert!(db::cluster::set_replicas(pool, "all").await.unwrap());
    let repo = create_repository(pool, &old, "moved").await;
    let uuid = rpc::repository_uuid(&repo.repository.disk_path).unwrap();
    repo.save("main", "README.md", "unborn", b"healthy").await;
    assert_current(&old, &new, &repo).await;
    pass(&copy).await;
    // The new primary takes the repository over and moves its copy into place.
    replicas::promote(pool, repo.id(), Some(new.context.node_id), false)
        .await
        .unwrap();
    pass(&new).await;
    // Its directory is damaged, so it answers with an unavailable error of its own.
    std::fs::write(primary_path(&new, uuid).join("config"), b"[broken").unwrap();
    old.shutdown().await;
    let blob = Operation::Blob {
        git_ref: "main".into(),
        path: "README.md".into(),
    };
    let (reply, body) = web
        .context
        .call(old.context.node_id, request(uuid, blob), b"")
        .await
        .unwrap();
    assert!(
        matches!(&reply.result,
            Err(Failure::Repository(error))
                if error.kind == ErrorKind::Unavailable && !error.is_storage_node_unavailable()),
        "the new primary answered, so its error is the answer to the read"
    );
    assert_ne!(
        body, b"healthy",
        "a copy must not answer for a primary that did"
    );
    web.shutdown().await;
    copy.shutdown().await;
    new.shutdown().await;
}

#[tokio::test]
async fn a_read_moves_on_when_the_own_copy_cannot_serve() {
    let database = native::PostgresDatabase::new().await;
    let root = tempfile::tempdir().unwrap();
    db::node_config::upsert_node_configuration(&database.pool, "public", true)
        .await
        .unwrap();
    assert!(db::cluster::set_replicas(&database.pool, "3")
        .await
        .unwrap());
    let primary = start_storage(&database.pool, root.path(), "store-a").await;
    let own = start_node(&database.pool, root.path(), "store-b", "web,storage").await;
    let other = start_storage(&database.pool, root.path(), "store-c").await;
    let repo = create_repository(&database.pool, &primary, "skip-own").await;
    let uuid = rpc::repository_uuid(&repo.repository.disk_path).unwrap();
    let head = repo.save("main", "README.md", "unborn", b"# skip\n").await;
    assert_current(&primary, &own, &repo).await;
    pass(&other).await;
    // The copy on this node is recorded but its files are gone.
    std::fs::remove_dir_all(copy_path(&own, uuid)).unwrap();
    let primary_id = primary.context.node_id;
    primary.shutdown().await;

    let input = fetch_input(&head);
    let mut pack: Vec<u8> = Vec::new();
    let (_, status) = own
        .context
        .git_session(
            primary_id,
            request(
                uuid,
                Operation::GitSession(GitSession::new(GitService::UploadPack, true)),
            ),
            GitStreams {
                input: &mut input.as_bytes(),
                output: &mut pack,
                errors: &mut tokio::io::sink(),
            },
        )
        .await
        .unwrap();
    assert!(status.success());
    assert!(!pack.is_empty());

    own.shutdown().await;
    other.shutdown().await;
}

#[tokio::test]
async fn writes_are_refused_while_the_primary_is_down() {
    let database = native::PostgresDatabase::new().await;
    let root = tempfile::tempdir().unwrap();
    let (primary, copy) = two_storage_nodes(&database.pool, root.path()).await;
    let web = web_node(&database.pool, root.path()).await;
    let repo = create_repository(&database.pool, &primary, "writes").await;
    let uuid = rpc::repository_uuid(&repo.repository.disk_path).unwrap();
    let head = repo
        .save("main", "README.md", "unborn", b"# writes\n")
        .await;
    assert_current(&primary, &copy, &repo).await;
    let (primary_id, copy_id) = (primary.context.node_id, copy.context.node_id);
    primary.shutdown().await;

    let create_branch = || Request {
        repository: uuid,
        operation: Operation::CreateBranch {
            name: "after".into(),
            sha: head.clone(),
        },
        actor: Some(repo.actor),
        ci_run: None,
    };
    let error = web
        .context
        .call(primary_id, create_branch(), b"")
        .await
        .unwrap_err();
    assert!(error.is_storage_node_unavailable());
    let error = web
        .context
        .git_session(
            primary_id,
            request(
                uuid,
                Operation::GitSession(GitSession::new(GitService::ReceivePack, true)),
            ),
            GitStreams {
                input: &mut tokio::io::empty(),
                output: &mut tokio::io::sink(),
                errors: &mut tokio::io::sink(),
            },
        )
        .await
        .unwrap_err();
    assert!(error
        .downcast::<RepoError>()
        .unwrap()
        .is_storage_node_unavailable());

    // The copy refuses a write sent to it directly, so nothing moves on the copy.
    let (reply, _) = web
        .context
        .call(copy_id, create_branch(), b"")
        .await
        .unwrap();
    assert!(
        matches!(reply.result, Err(Failure::Repository(error)) if error.kind == ErrorKind::Invalid)
    );
    let copied = RepoStore::from_disk_path(copy_path(&copy, uuid).to_str().unwrap());
    assert!(!copied
        .refs()
        .await
        .unwrap()
        .contains_key("refs/heads/after"));

    web.shutdown().await;
    copy.shutdown().await;
}

#[tokio::test]
async fn a_node_without_a_copy_row_refuses_reads() {
    let database = native::PostgresDatabase::new().await;
    let root = tempfile::tempdir().unwrap();
    let (primary, copy) = two_storage_nodes(&database.pool, root.path()).await;
    let web = web_node(&database.pool, root.path()).await;
    let repo = create_repository(&database.pool, &primary, "outside").await;
    let uuid = rpc::repository_uuid(&repo.repository.disk_path).unwrap();
    repo.save("main", "a.txt", "unborn", b"a").await;
    // Assign the copy while there is only one eligible holder.
    assert_current(&primary, &copy, &repo).await;
    let outsider = start_storage(&database.pool, root.path(), "store-c").await;
    assert_eq!(applied_on(&database.pool, repo.id(), &outsider).await, None);
    primary.shutdown().await;

    // The primary is down, and the outsider holds no copy, so it refuses the read.
    let (reply, _) = web
        .context
        .call(
            outsider.context.node_id,
            request(uuid, Operation::Refs),
            b"",
        )
        .await
        .unwrap();
    assert!(
        matches!(reply.result, Err(Failure::Repository(error)) if error.kind == ErrorKind::Invalid)
    );

    web.shutdown().await;
    outsider.shutdown().await;
    copy.shutdown().await;
}

#[tokio::test]
async fn a_read_without_a_copy_that_has_copied_is_unavailable() {
    let database = native::PostgresDatabase::new().await;
    let root = tempfile::tempdir().unwrap();
    let (primary, copy) = two_storage_nodes(&database.pool, root.path()).await;
    let web = web_node(&database.pool, root.path()).await;
    // The copy row exists, but the copy has never been brought up to date.
    let copying = create_repository(&database.pool, &primary, "copying").await;
    let copying_uuid = rpc::repository_uuid(&copying.repository.disk_path).unwrap();
    copying.save("main", "a.txt", "unborn", b"a").await;
    pass(&primary).await;
    assert_eq!(
        applied_on(&database.pool, copying.id(), &copy).await,
        Some(-1)
    );
    // This repository has no copy row at all.
    let uncopied = create_repository(&database.pool, &primary, "uncopied").await;
    let uncopied_uuid = rpc::repository_uuid(&uncopied.repository.disk_path).unwrap();
    let primary_id = primary.context.node_id;
    primary.shutdown().await;

    for uuid in [copying_uuid, uncopied_uuid] {
        let error = web
            .context
            .call(primary_id, request(uuid, Operation::Refs), b"")
            .await
            .unwrap_err();
        assert!(error.is_storage_node_unavailable());
    }

    web.shutdown().await;
    copy.shutdown().await;
}

#[tokio::test]
async fn a_not_found_from_the_primary_does_not_fail_over() {
    let database = native::PostgresDatabase::new().await;
    let root = tempfile::tempdir().unwrap();
    let (primary, copy) = two_storage_nodes(&database.pool, root.path()).await;
    let web = web_node(&database.pool, root.path()).await;
    let repo = create_repository(&database.pool, &primary, "answers").await;
    let uuid = rpc::repository_uuid(&repo.repository.disk_path).unwrap();
    let head = repo
        .save("main", "README.md", "unborn", b"# answers\n")
        .await;
    repo.branch("gone", &head).await;
    assert_current(&primary, &copy, &repo).await;
    // The branch disappears from the primary without a write, so the copy still has it.
    // Only a failover to the copy could find it.
    let git = git2::Repository::open_bare(primary_path(&primary, uuid)).unwrap();
    git.find_reference("refs/heads/gone")
        .unwrap()
        .delete()
        .unwrap();

    let (reply, body) = web
        .context
        .call(
            primary.context.node_id,
            request(
                uuid,
                Operation::Blob {
                    git_ref: "gone".into(),
                    path: "README.md".into(),
                },
            ),
            b"",
        )
        .await
        .unwrap();
    assert!(body.is_empty());
    assert!(
        matches!(reply.result, Err(Failure::Repository(error)) if error.kind == ErrorKind::NotFound)
    );

    web.shutdown().await;
    copy.shutdown().await;
    primary.shutdown().await;
}
