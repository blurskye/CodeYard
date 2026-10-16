//! Merge and fork tests on real Git repositories, with a throwaway PostgreSQL
//! schema for the database parts. A missing database is a failure, not a skip.
#[path = "support/native_database.rs"]
mod native;

use codeyard::backend::repositories::store::{MergeMethod, MergeRequest};
use codeyard::backend::{
    db::{self, repositories},
    repositories::{lifecycle, store::RepoStore},
};
use git2::{Oid, Repository, Signature};
use std::{path::Path, sync::Arc};
use uuid::Uuid;

/// A merge commit of `head` into `base`, committed by `name` and `email`.
fn merge_request(head: &str, base: &str, message: &str, name: &str, email: &str) -> MergeRequest {
    MergeRequest {
        head_branch: "feature".into(),
        base_branch: "main".into(),
        head: head.into(),
        base: base.into(),
        message: message.into(),
        name: name.into(),
        email: email.into(),
        method: MergeMethod::Merge,
        author_name: None,
        author_email: None,
    }
}

fn store(path: &Path) -> RepoStore {
    RepoStore::from_disk_path(path.to_str().unwrap())
}

async fn publication_authority(path: &Path) -> (native::NativeDatabase, i64, i64) {
    let database = native::NativeDatabase::new().await;
    let actor = db::users::accounts::create_user(
        &database.pool,
        &format!("publisher_{}", Uuid::new_v4().simple()),
        None,
    )
    .await
    .unwrap();
    let repository = db::repositories::records::create_repository(
        &database.pool,
        actor.id,
        &format!("repo_{}", Uuid::new_v4().simple()),
        None,
        false,
        path.to_str().unwrap(),
    )
    .await
    .unwrap();
    (database, repository.id, actor.id)
}

fn commit(repo: &Repository, branch: &str, parent: Option<Oid>, file: &str, content: &[u8]) -> Oid {
    let parent = parent.map(|oid| repo.find_commit(oid).unwrap());
    let tree = parent.as_ref().map(|parent| parent.tree().unwrap());
    let mut builder = repo.treebuilder(tree.as_ref()).unwrap();
    builder
        .insert(file, repo.blob(content).unwrap(), 0o100644)
        .unwrap();
    let tree = repo.find_tree(builder.write().unwrap()).unwrap();
    let author = Signature::now("fixture", "fixture@example.invalid").unwrap();
    repo.commit(
        Some(&format!("refs/heads/{branch}")),
        &author,
        &author,
        file,
        &tree,
        &parent.iter().collect::<Vec<_>>(),
    )
    .unwrap()
}
fn tip(path: &Path, branch: &str) -> Oid {
    Repository::open_bare(path)
        .unwrap()
        .find_reference(&format!("refs/heads/{branch}"))
        .unwrap()
        .target()
        .unwrap()
}
fn graph(path: &Path) -> (Oid, Oid, Oid) {
    let repo = Repository::init_bare(path).unwrap();
    let ancestor = commit(&repo, "main", None, "common", b"ancestor");
    let base = commit(&repo, "main", Some(ancestor), "base-only", b"base change");
    let head = commit(
        &repo,
        "feature",
        Some(ancestor),
        "head-only",
        b"reviewed H1",
    );
    (ancestor, base, head)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn divergent_merge_retains_both_contents_and_both_parents() {
    let temp = tempfile::tempdir().unwrap();
    let (_, base, head) = graph(temp.path());
    let (database, repo_id, actor) = publication_authority(temp.path()).await;
    let result = store(temp.path())
        .prepare_merge(&merge_request(
            &head.to_string(),
            &base.to_string(),
            "merge",
            "fixture",
            "fixture@example.invalid",
        ))
        .await
        .unwrap();
    assert_eq!(
        tip(temp.path(), "main"),
        base,
        "preparation must not publish"
    );
    store(temp.path())
        .publish_merge_as(
            &database.pool,
            repo_id,
            actor,
            "feature",
            "main",
            &head.to_string(),
            &base.to_string(),
            &result,
            MergeMethod::Merge,
        )
        .await
        .unwrap();
    let repo = Repository::open_bare(temp.path()).unwrap();
    let merged = repo.find_commit(Oid::from_str(&result).unwrap()).unwrap();
    assert_eq!(merged.parent_ids().collect::<Vec<_>>(), vec![base, head]);
    let tree = merged.tree().unwrap();
    for (file, expected) in [
        ("base-only", b"base change".as_slice()),
        ("head-only", b"reviewed H1".as_slice()),
    ] {
        let entry = tree.get_path(Path::new(file)).unwrap();
        assert_eq!(repo.find_blob(entry.id()).unwrap().content(), expected);
    }
    assert!(repo.graph_descendant_of(merged.id(), base).unwrap());
    assert!(repo.graph_descendant_of(merged.id(), head).unwrap());
    database.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn movement_after_preparation_cannot_publish_h2_or_overwrite_b2() {
    for move_head in [true, false] {
        let temp = tempfile::tempdir().unwrap();
        let (_, base, head) = graph(temp.path());
        let (database, repo_id, actor) = publication_authority(temp.path()).await;
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let writer_barrier = barrier.clone();
        let path = temp.path().to_path_buf();
        let writer = tokio::spawn(async move {
            writer_barrier.wait().await;
            let repo = Repository::open_bare(&path).unwrap();
            let branch = if move_head { "feature" } else { "main" };
            commit(
                &repo,
                branch,
                Some(if move_head { head } else { base }),
                "unreviewed-H2-or-concurrent-B2",
                b"must not be overwritten",
            )
        });
        let result = store(temp.path())
            .prepare_merge(&merge_request(
                &head.to_string(),
                &base.to_string(),
                "merge H1",
                "fixture",
                "fixture@example.invalid",
            ))
            .await
            .unwrap();
        barrier.wait().await;
        let moved = writer.await.unwrap();
        assert!(store(temp.path())
            .publish_merge_as(
                &database.pool,
                repo_id,
                actor,
                "feature",
                "main",
                &head.to_string(),
                &base.to_string(),
                &result,
                MergeMethod::Merge
            )
            .await
            .is_err());
        assert_eq!(
            tip(temp.path(), "main"),
            if move_head { base } else { moved }
        );
        assert_eq!(
            tip(temp.path(), "feature"),
            if move_head { moved } else { head }
        );
        database.close().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn competing_merge_publications_have_one_winner() {
    let temp = tempfile::tempdir().unwrap();
    let (_, base, head) = graph(temp.path());
    let (database, repo_id, actor) = publication_authority(temp.path()).await;
    let result = store(temp.path())
        .prepare_merge(&merge_request(
            &head.to_string(),
            &base.to_string(),
            "merge",
            "fixture",
            "fixture@example.invalid",
        ))
        .await
        .unwrap();
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let publish = || {
        let path = temp.path().to_path_buf();
        let barrier = barrier.clone();
        let result = result.clone();
        let pool = database.pool.clone();
        tokio::spawn(async move {
            barrier.wait().await;
            store(&path)
                .publish_merge_as(
                    &pool,
                    repo_id,
                    actor,
                    "feature",
                    "main",
                    &head.to_string(),
                    &base.to_string(),
                    &result,
                    MergeMethod::Merge,
                )
                .await
        })
    };
    let first = publish();
    let second = publish();
    let (first, second) = tokio::join!(first, second);
    assert_ne!(first.unwrap().is_ok(), second.unwrap().is_ok());
    assert_eq!(tip(temp.path(), "main").to_string(), result);
    database.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn up_to_date_merge_makes_a_merge_commit_and_conflicts_are_honest() {
    let temp = tempfile::tempdir().unwrap();
    let repo = Repository::init_bare(temp.path()).unwrap();
    let (database, repo_id, actor) = publication_authority(temp.path()).await;
    let base = commit(&repo, "main", None, "file", b"ancestor\n");
    let head = commit(&repo, "feature", Some(base), "file", b"head\n");
    let result = store(temp.path())
        .prepare_merge(&merge_request(
            &head.to_string(),
            &base.to_string(),
            "merge of an up-to-date head",
            "fixture",
            "fixture@example.invalid",
        ))
        .await
        .unwrap();
    // The head already contains the base, but the merge still records both parents.
    let merged = repo.find_commit(Oid::from_str(&result).unwrap()).unwrap();
    assert_eq!(merged.parent_ids().collect::<Vec<_>>(), vec![base, head]);
    assert_eq!(merged.tree_id(), repo.find_commit(head).unwrap().tree_id());
    store(temp.path())
        .publish_merge_as(
            &database.pool,
            repo_id,
            actor,
            "feature",
            "main",
            &head.to_string(),
            &base.to_string(),
            &result,
            MergeMethod::Merge,
        )
        .await
        .unwrap();
    assert_eq!(tip(temp.path(), "main"), Oid::from_str(&result).unwrap());
    let divergent = commit(&repo, "other", Some(base), "file", b"different\n");
    assert!(store(temp.path())
        .prepare_merge(&merge_request(
            &divergent.to_string(),
            &head.to_string(),
            "conflict",
            "fixture",
            "fixture@example.invalid"
        ))
        .await
        .is_err());
    assert_eq!(tip(temp.path(), "main"), Oid::from_str(&result).unwrap());
    let missing = temp.path().join("missing.git");
    assert!(store(&missing)
        .prepare_merge(&merge_request(
            &divergent.to_string(),
            &head.to_string(),
            "error",
            "fixture",
            "fixture@example.invalid"
        ))
        .await
        .is_err());
    database.close().await;
}

struct TestDb {
    pool: db::DbPool,
    raw: sqlx::PgPool,
    schema: String,
    url: String,
}
impl Drop for TestDb {
    fn drop(&mut self) {
        let fixture_pool = self.pool.clone();
        let schema = self.schema.clone();
        let url = self.url.clone();
        let result = std::thread::spawn(move || {
            tokio::runtime::Runtime::new()
                .unwrap()
                .block_on(async move {
                    tokio::time::timeout(std::time::Duration::from_secs(10), fixture_pool.close())
                        .await
                        .map_err(|_| sqlx::Error::PoolTimedOut)?;
                    let options: sqlx::postgres::PgConnectOptions = url.parse()?;
                    let pool = sqlx::postgres::PgPoolOptions::new()
                        .max_connections(1)
                        .connect_with(
                            options.options([("lock_timeout", "5s"), ("statement_timeout", "10s")]),
                        )
                        .await?;
                    let result = sqlx::query(&format!("DROP SCHEMA \"{schema}\" CASCADE"))
                        .execute(&pool)
                        .await;
                    pool.close().await;
                    result.map(|_| ()).map_err(anyhow::Error::from)
                })
        })
        .join();
        if !matches!(result, Ok(Ok(()))) {
            if std::thread::panicking() {
                eprintln!("production_git fixture cleanup failed");
            } else {
                panic!("production_git fixture cleanup failed");
            }
        }
    }
}
async fn database() -> TestDb {
    let url = std::env::var("GITSERV_TEST_DATABASE_URL")
        .expect("required disposable PostgreSQL fixture URL");
    let parsed = url::Url::parse(&url).expect("valid fixture URL");
    assert!(
        matches!(parsed.scheme(), "postgres" | "postgresql")
            && parsed.path() == "/gitserv_test"
            && parsed.query().is_none()
            && parsed.fragment().is_none(),
        "only explicitly owned gitserv_test DB is allowed"
    );
    let schema = format!("production_git_{}", Uuid::new_v4().simple());
    let admin = sqlx::postgres::PgPoolOptions::new()
        .connect(&url)
        .await
        .expect("required disposable PostgreSQL is unavailable");
    sqlx::query(&format!("CREATE SCHEMA \"{schema}\""))
        .execute(&admin)
        .await
        .unwrap();
    let mut fixture = TestDb {
        pool: admin.clone().into(),
        raw: admin.clone(),
        schema,
        url,
    };
    admin.close().await;
    fixture.raw = sqlx::postgres::PgPoolOptions::new()
        .connect(&format!(
            "{}?options=-csearch_path%3D{}",
            fixture.url, fixture.schema
        ))
        .await
        .unwrap();
    fixture.pool = fixture.raw.clone().into();
    db::run_migrations(&fixture.pool)
        .await
        .expect("required fixture migrations");
    fixture
}
async fn user(pool: &db::DbPool, label: &str) -> db::models::User {
    db::users::accounts::create_user(pool, &format!("{label}_{}", Uuid::new_v4().simple()), None)
        .await
        .unwrap()
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn private_forks_and_descendants_cannot_be_published_through_settings() {
    let fixture = database().await;
    let pool = &fixture.pool;
    let owner = user(pool, "owner").await;
    let reader = user(pool, "reader").await;
    let other = user(pool, "unrelated").await;
    let temp = tempfile::tempdir().unwrap();
    let source = db::repositories::records::create_repository(
        pool,
        owner.id,
        "private",
        None,
        true,
        temp.path().to_str().unwrap(),
    )
    .await
    .unwrap();
    db::repositories::access::grant_repo_access(
        pool,
        reader.id,
        source.id,
        db::models::AccessLevel::Read,
    )
    .await
    .unwrap();
    let fork = db::repositories::discovery::create_forked_repository(
        pool,
        db::repositories::discovery::NewForkedRepository {
            owner_id: reader.id,
            name: "fork",
            description: None,
            is_private: false,
            disk_path: "/fixture/fork",
            forked_from: source.id,
            default_branch: "main",
        },
    )
    .await
    .unwrap();
    assert!(
        fork.is_private,
        "even a legacy caller requesting public must inherit the floor"
    );
    assert_eq!(
        db::repositories::access::get_user_repo_access(pool, other.id, fork.id)
            .await
            .unwrap(),
        db::models::AccessLevel::None
    );
    assert!(db::repositories::records::update_repository(
        pool,
        reader.id,
        fork.id,
        None,
        false,
        db::repositories::records::MergeMethodUpdate {
            merge: None,
            squash: None,
            rebase: None
        }
    )
    .await
    .is_err());
    assert!(sqlx::query(
        "UPDATE repositories SET is_private=FALSE, visibility_locked=FALSE WHERE id=$1"
    )
    .bind(fork.id)
    .execute(&fixture.raw)
    .await
    .is_err());
    let child = db::repositories::discovery::create_forked_repository(
        pool,
        db::repositories::discovery::NewForkedRepository {
            owner_id: owner.id,
            name: "child",
            description: None,
            is_private: false,
            disk_path: "/fixture/child",
            forked_from: fork.id,
            default_branch: "main",
        },
    )
    .await
    .unwrap();
    assert!(child.is_private);
    assert!(db::repositories::records::update_repository(
        pool,
        owner.id,
        child.id,
        None,
        false,
        db::repositories::records::MergeMethodUpdate {
            merge: None,
            squash: None,
            rebase: None
        }
    )
    .await
    .is_err());
    let denied = repositories::insert_repository(
        pool,
        repositories::NewRepository {
            owner: repositories::RepositoryCreationOwner::personal(other.id),
            name: "unrelated-copy",
            description: None,
            is_private: false,
            disk_path: "/fixture/denied",
            storage_node_id: None,
            source_id: Some(source.id),
        },
    )
    .await;
    assert!(
        denied.is_err(),
        "forking a private source needs read access"
    );
    db::repositories::records::delete_repository(pool, owner.id, source.id)
        .await
        .unwrap();
    assert!(db::repositories::records::update_repository(
        pool,
        reader.id,
        fork.id,
        None,
        false,
        db::repositories::records::MergeMethodUpdate {
            merge: None,
            squash: None,
            rebase: None
        }
    )
    .await
    .is_err());
    assert_eq!(
        db::repositories::access::get_user_repo_access(pool, other.id, child.id)
            .await
            .unwrap(),
        db::models::AccessLevel::None
    );
    let public = db::repositories::records::create_repository(
        pool,
        owner.id,
        "initially-public",
        None,
        false,
        "/fixture/public",
    )
    .await
    .unwrap();
    let formerly_public = db::repositories::discovery::create_forked_repository(
        pool,
        db::repositories::discovery::NewForkedRepository {
            owner_id: reader.id,
            name: "formerly-public",
            description: None,
            is_private: false,
            disk_path: "/fixture/public-fork",
            forked_from: public.id,
            default_branch: "main",
        },
    )
    .await
    .unwrap();
    assert!(!formerly_public.is_private);
    db::repositories::records::update_repository(
        pool,
        owner.id,
        public.id,
        None,
        true,
        db::repositories::records::MergeMethodUpdate {
            merge: None,
            squash: None,
            rebase: None,
        },
    )
    .await
    .unwrap();
    assert!(
        db::repositories::records::get_repository_by_id(pool, formerly_public.id)
            .await
            .unwrap()
            .unwrap()
            .is_private
    );
    assert!(db::repositories::records::update_repository(
        pool,
        reader.id,
        formerly_public.id,
        None,
        false,
        db::repositories::records::MergeMethodUpdate {
            merge: None,
            squash: None,
            rebase: None
        }
    )
    .await
    .is_err());
    // No claim that a recipient who could clone can be prevented from exporting.
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn duplicate_fork_loser_never_touches_winner() {
    let fixture = database().await;
    let pool = &fixture.pool;
    let owner = user(pool, "owner").await;
    let reader = user(pool, "reader").await;
    let temp = tempfile::tempdir().unwrap();
    let source_path = temp.path().join("source.git");
    graph(&source_path);
    let source = db::repositories::records::create_repository(
        pool,
        owner.id,
        "source",
        None,
        false,
        source_path.to_str().unwrap(),
    )
    .await
    .unwrap();
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let attempt = |operation: Uuid| {
        let pool = pool.clone();
        let barrier = barrier.clone();
        let source_id = source.id;
        let reader_id = reader.id;
        let path = temp.path().join(operation.to_string());
        tokio::spawn(async move {
            barrier.wait().await;
            let result = repositories::insert_repository(
                &pool,
                repositories::NewRepository {
                    owner: repositories::RepositoryCreationOwner::personal(reader_id),
                    name: "source",
                    description: None,
                    is_private: false,
                    disk_path: path.to_str().unwrap(),
                    storage_node_id: None,
                    source_id: Some(source_id),
                },
            )
            .await
            .unwrap();
            (operation, path, result)
        })
    };
    let first = attempt(Uuid::new_v4());
    let second = attempt(Uuid::new_v4());
    let outcomes = [first.await.unwrap(), second.await.unwrap()];
    assert_eq!(
        outcomes
            .iter()
            .filter(|(_, _, repo)| repo.is_some())
            .count(),
        1
    );
    let (_, path, winner) = outcomes.iter().find(|(_, _, repo)| repo.is_some()).unwrap();
    let winner = winner.as_ref().unwrap();
    std::fs::create_dir(path).unwrap();
    store(path)
        .initialize(Some(&store(&source_path)), "main")
        .await
        .unwrap();
    assert_eq!(tip(path, "main"), tip(&source_path, "main"));
    let (_, loser_path, _) = outcomes.iter().find(|(_, _, repo)| repo.is_none()).unwrap();
    assert!(!loser_path.exists());
    assert!(
        path.join("objects").is_dir(),
        "winner storage must survive the losing request"
    );
    db::repositories::records::delete_repository(pool, owner.id, source.id)
        .await
        .unwrap();
    assert!(
        !db::repositories::records::get_repository_by_id(pool, winner.id)
            .await
            .unwrap()
            .unwrap()
            .is_private,
        "deleting a public source is not a policy declassification/narrowing event"
    );
}

// Each fork updates the source's fork count; forks started together must queue
// on that row instead of deadlocking.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn simultaneous_forks_of_one_source_by_different_users_all_succeed() {
    let fixture = database().await;
    let owner = user(&fixture.pool, "owner").await;
    let source = db::repositories::records::create_repository(
        &fixture.pool,
        owner.id,
        "source",
        None,
        false,
        "/fixture/source",
    )
    .await
    .unwrap();
    const FORKS: usize = 4;
    let barrier = Arc::new(tokio::sync::Barrier::new(FORKS));
    let mut tasks = Vec::new();
    for index in 0..FORKS {
        let forker = user(&fixture.pool, "forker").await;
        let pool = fixture.pool.clone();
        let barrier = barrier.clone();
        let source_id = source.id;
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            let disk_path = format!("/fixture/fork-{index}");
            repositories::insert_repository(
                &pool,
                repositories::NewRepository {
                    owner: repositories::RepositoryCreationOwner::personal(forker.id),
                    name: "fork",
                    description: None,
                    is_private: false,
                    disk_path: &disk_path,
                    storage_node_id: None,
                    source_id: Some(source_id),
                },
            )
            .await
        }));
    }
    for task in tasks {
        assert!(task.await.unwrap().unwrap().is_some());
    }
    let source = db::repositories::records::get_repository_by_id(&fixture.pool, source.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(source.forks_count, FORKS as i32);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_concurrent_fork_domain_preserves_winner_and_does_not_copy_hooks_or_config() {
    let fixture = database().await;
    let owner = user(&fixture.pool, "owner").await;
    let reader = user(&fixture.pool, "reader").await;
    let temp = tempfile::tempdir().unwrap();
    let source_path = temp.path().join("source.git");
    graph(&source_path);
    let source = db::repositories::records::create_repository(
        &fixture.pool,
        owner.id,
        "source",
        None,
        false,
        source_path.to_str().unwrap(),
    )
    .await
    .unwrap();
    std::fs::write(
        source_path.join("hooks/post-update"),
        b"must not copy source hook",
    )
    .unwrap();
    Repository::open_bare(&source_path)
        .unwrap()
        .config()
        .unwrap()
        // Safe source configuration is deliberately different from the fresh
        // initializer's own config. Unsupported sections are refused by the
        // registration layout tests, not smuggled into a successful fork fixture.
        .set_str("core.filemode", "false")
        .unwrap();
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let attempt = || {
        let pool = fixture.pool.clone();
        // The actual configured root exists and owns BOTH source and target.
        let root = temp.path().to_owned();
        let source = source.clone();
        let barrier = barrier.clone();
        let actor = reader.id;
        tokio::spawn(async move {
            barrier.wait().await;
            lifecycle::provision(
                (&pool, None),
                &root,
                repositories::RepositoryCreationOwner::personal(actor),
                "source",
                None,
                false,
                Some(&source),
            )
            .await
        })
    };
    let first = attempt();
    let second = attempt();
    let results = [first.await.unwrap(), second.await.unwrap()];
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .find_map(|result| result.as_ref().err())
            .unwrap()
            .kind,
        lifecycle::LifecycleFailure::Conflict
    );
    let winner = results
        .iter()
        .find_map(|result| result.as_ref().ok())
        .unwrap();
    let path = Path::new(&winner.disk_path);
    assert_eq!(tip(path, "main"), tip(&source_path, "main"));
    assert!(!path.join("hooks/post-update").exists());
    assert!(Repository::open_bare(path)
        .unwrap()
        .config()
        .unwrap()
        .get_bool("core.filemode")
        .unwrap());
    assert!(!Repository::open_bare(&source_path)
        .unwrap()
        .config()
        .unwrap()
        .get_bool("core.filemode")
        .unwrap());
    // The loser's half-made directory is gone: only the source and the winner remain.
    let mut repositories: Vec<_> = std::fs::read_dir(temp.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    repositories.sort();
    let mut expected = vec![
        std::ffi::OsString::from("source.git"),
        path.file_name().unwrap().to_owned(),
    ];
    expected.sort();
    assert_eq!(repositories, expected);
    assert!(
        db::repositories::records::get_repository(&fixture.pool, &reader.username, "source")
            .await
            .unwrap()
            .is_some()
    );
}
