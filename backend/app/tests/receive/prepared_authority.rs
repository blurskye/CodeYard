//! Live publication authority at the reference-transaction Prepared seam.
//!
//! Each test lets pre-receive succeed, holds the receive after its durability barrier and
//! before its Prepared authorization, changes live authority, and only then lets Git try to
//! publish. A rejection must leave the refs unchanged and release the repository lease.

use anyhow::ensure;
use anyhow::Result;
use codeyard_ci::runs::CiRuns;
use codeyard_database::models::AccessLevel;
use codeyard_database::DbPool;
use codeyard_git::protocol::ZERO_SHA;
use codeyard_git::ref_names::MAX_REF_BYTES;
use codeyard_git_storage::repositories::git_sessions::hook_barrier::hold_next_hook;
use codeyard_git_storage::repositories::git_sessions::hook_barrier::Phase;
use codeyard_git_storage::repositories::git_sessions::receive_session;
use codeyard_git_storage::repositories::git_sessions::ReceiveCredential;
use codeyard_git_storage::store_types::GitService;
use codeyard_git_storage::store_types::GitSession;
use codeyard_git_storage::store_types::GitStreams;
use codeyard_notifications::broadcast::NotificationBroadcaster;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;
use tokio::time::timeout;

use super::policy_tests::fixture::receive_request;
use super::policy_tests::fixture::receive_requests;

#[derive(Clone, Copy)]
enum Backend {
    Sqlite,
    Postgres,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Case {
    Token,
    User,
    Member,
    Protected,
    Allowed,
}

impl Case {
    const ALL: [Case; 5] = [
        Case::Token,
        Case::User,
        Case::Member,
        Case::Protected,
        Case::Allowed,
    ];

    fn name(self) -> &'static str {
        match self {
            Self::Token => "revoked-token",
            Self::User => "banned-user",
            Self::Member => "removed-member",
            Self::Protected => "new-branch-protection",
            Self::Allowed => "allowed",
        }
    }
}

struct Fixture {
    _directory: TempDir,
    // The PostgreSQL helper drops its schema with this owner.
    _database: Option<crate::Database>,
    path: PathBuf,
    pool: DbPool,
    ci: Arc<CiRuns>,
    repo_id: i64,
    owner_id: i64,
    actor_id: i64,
    session: String,
    token_id: i64,
    old: git2::Oid,
    new: git2::Oid,
    request: Vec<u8>,
}

impl Fixture {
    async fn new(backend: Backend, case: usize) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let (pool, database) = match backend {
            Backend::Sqlite => {
                let native = sqlx::sqlite::SqlitePoolOptions::new()
                    .max_connections(4)
                    .connect_with(
                        sqlx::sqlite::SqliteConnectOptions::new()
                            .filename(directory.path().join(format!("application-{case}.sqlite")))
                            .create_if_missing(true)
                            .foreign_keys(true),
                    )
                    .await
                    .map(codeyard_database::pool::Pool::from)
                    .unwrap();
                let pool = DbPool::from(native);
                codeyard_database::run_migrations(&pool).await.unwrap();
                (pool, None)
            }
            Backend::Postgres => {
                let database = crate::database().await;
                (database.pool.clone(), Some(database))
            }
        };
        let owner = codeyard_db_identity::users::accounts::create_user(
            &pool,
            &format!("owner{case}"),
            None,
        )
        .await
        .unwrap();
        let actor = codeyard_db_identity::users::accounts::create_user(
            &pool,
            &format!("writer{case}"),
            None,
        )
        .await
        .unwrap();
        let session = format!("{:064x}", 0x2000_u64 + case as u64);
        codeyard_db_identity::users::sessions::create_session(
            &pool,
            &session,
            actor.id,
            None,
            None,
            chrono::Utc::now() + chrono::Duration::hours(1),
        )
        .await
        .unwrap();
        let token = codeyard_db_identity::users::tokens::create_token(
            &pool,
            actor.id,
            &session,
            "writer",
            &format!("{:064x}", 0x1000_u64 + case as u64),
            "write",
            None,
        )
        .await
        .unwrap()
        .unwrap();
        let path = directory.path().join(format!("repository-{case}.git"));
        let mut options = git2::RepositoryInitOptions::new();
        options.bare(true).initial_head("main");
        let bare = git2::Repository::init_opts(&path, &options).unwrap();
        let path = std::fs::canonicalize(path).unwrap();
        let tree_oid = bare.treebuilder(None).unwrap().write().unwrap();
        let tree = bare.find_tree(tree_oid).unwrap();
        let signature = git2::Signature::now("Fixture", "fixture@example.invalid").unwrap();
        let old = bare
            .commit(
                Some("refs/heads/main"),
                &signature,
                &signature,
                "fixture",
                &tree,
                &[],
            )
            .unwrap();
        bare.reference("refs/heads/topic", old, false, "topic")
            .unwrap();
        let parent = bare.find_commit(old).unwrap();
        let new = bare
            .commit(
                None,
                &signature,
                &signature,
                "push",
                &parent.tree().unwrap(),
                &[&parent],
            )
            .unwrap();
        let repo = codeyard_db_repos::repositories::records::create_repository(
            &pool,
            owner.id,
            &format!("repo{case}"),
            None,
            true,
            path.to_str().unwrap(),
        )
        .await
        .unwrap();
        codeyard_db_repos::repositories::access::grant_repo_access(
            &pool,
            actor.id,
            repo.id,
            AccessLevel::Write,
        )
        .await
        .unwrap();
        let ci = Arc::new(CiRuns::new(
            pool.clone(),
            Arc::new(codeyard_ci::config::CiConfig::default()),
            Arc::new(tokio::sync::Notify::new()),
            None,
            Arc::new(NotificationBroadcaster::new(&pool)),
        ));
        let request = receive_request(
            &path,
            "refs/heads/topic",
            &old.to_string(),
            &new.to_string(),
        );
        Self {
            _directory: directory,
            _database: database,
            path,
            pool,
            ci,
            repo_id: repo.id,
            owner_id: owner.id,
            actor_id: actor.id,
            session,
            token_id: token.id,
            old,
            new,
            request,
        }
    }

    fn receive(&self) -> tokio::task::JoinHandle<Result<Vec<u8>>> {
        let pool = self.pool.clone();
        let ci = self.ci.clone();
        let path = self.path.clone();
        let request = self.request.clone();
        let repo_id = self.repo_id;
        let credential = ReceiveCredential::Token(self.token_id);
        tokio::spawn(async move {
            let mut session = GitSession::new(GitService::ReceivePack, true);
            session.credential = Some(credential);
            session.owner = "owner".into();
            let mut output = Vec::new();
            let status = receive_session(
                pool,
                ci,
                codeyard_git_storage::repositories::git_sessions::ReceiveSession {
                    repo_id,
                    path: &path,
                    executable: std::env::var_os("GITSERV_TEST_OPENSSH_HELPER")
                        .expect("set GITSERV_TEST_OPENSSH_HELPER to the integrated application")
                        .into(),
                    session,
                },
                GitStreams {
                    input: &mut request.as_slice(),
                    output: &mut output,
                    errors: &mut tokio::io::sink(),
                },
            )
            .await?;
            ensure!(status.success(), "Git receive failed");
            Ok(output)
        })
    }

    async fn revoke(&self, case: Case) {
        match case {
            Case::Token => {
                assert!(codeyard_db_identity::users::tokens::delete_token(
                    &self.pool,
                    self.actor_id,
                    &self.session,
                    self.token_id,
                )
                .await
                .unwrap());
            }
            Case::User => match &self.pool {
                DbPool::Sqlite(pool) => {
                    sqlx::query("UPDATE users SET is_banned=TRUE WHERE id=?")
                        .bind(self.actor_id)
                        .execute(pool)
                        .await
                        .unwrap();
                }
                DbPool::Postgres(pool) => {
                    sqlx::query("UPDATE users SET is_banned=TRUE WHERE id=$1")
                        .bind(self.actor_id)
                        .execute(pool)
                        .await
                        .unwrap();
                }
            },
            Case::Member => {
                assert!(codeyard_db_repos::repositories::access::revoke_repo_access(
                    &self.pool,
                    self.actor_id,
                    self.repo_id,
                )
                .await
                .unwrap());
            }
            Case::Protected => {
                codeyard_db_repos::repositories::protection::upsert_branch_protection_rule(
                    &self.pool,
                    codeyard_db_repos::repositories::protection::BranchProtectionRuleUpsert {
                        actor_id: self.owner_id,
                        repo_id: self.repo_id,
                        pattern: "topic",
                        require_pull_request: true,
                        required_approving_reviews: 1,
                        dismiss_stale_reviews: true,
                        require_status_checks: false,
                        required_status_checks: vec![],
                        require_up_to_date_branch: false,
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
            }
            Case::Allowed => {}
        }
    }
}

/// Pre-receive succeeds, the receive is held after its durability barrier, authority changes,
/// and Git publishes only after the release. The refs must not move for a rejection, and the
/// repository lease must be available again.
async fn contract(backend: Backend) {
    for (index, case) in Case::ALL.into_iter().enumerate() {
        let fixture = Fixture::new(backend, index).await;
        let (reached, release) = hold_next_hook(fixture.path.clone(), Phase::Prepared);
        let mut task = fixture.receive();
        let barrier = timeout(Duration::from_secs(20), reached);
        tokio::pin!(barrier);
        let reached = tokio::select! {
            result = &mut barrier => match result {
                Ok(reached) => {
                    reached.expect("publication barrier dropped");
                    true
                }
                Err(_) => false,
            },
            finished = &mut task => {
                panic!(
                    "{}: push ended before its Prepared check: {finished:?}",
                    case.name()
                );
            }
        };
        if !reached {
            // Keep ownership: a timed-out push must not be detached while it may hold the
            // repository lease and a Git process slot.
            task.abort();
            let _ = timeout(Duration::from_secs(5), &mut task).await;
            panic!("{}: push never reached Prepared", case.name());
        }
        fixture.revoke(case).await;
        release.send(()).unwrap();
        let finished = match timeout(Duration::from_secs(20), &mut task).await {
            Ok(joined) => joined.expect("receive task panicked"),
            Err(_) => {
                task.abort();
                let _ = timeout(Duration::from_secs(5), &mut task).await;
                panic!(
                    "{}: push did not finish; its publication lease or Git slot leaked",
                    case.name()
                );
            }
        };
        let bare = git2::Repository::open_bare(&fixture.path).unwrap();
        let topic = bare.find_reference("refs/heads/topic").unwrap().target();
        if case == Case::Allowed {
            let pushed = finished.expect("allowed push failed");
            let output = String::from_utf8_lossy(&pushed);
            assert!(
                output.contains("ok refs/heads/topic"),
                "{}: {output}",
                case.name()
            );
            assert_eq!(topic, Some(fixture.new), "{}", case.name());
        } else {
            // A prepared-hook refusal is fatal, not an in-band rejection: receive-pack exits
            // nonzero with no valid report-status. The receive helper therefore surfaces an
            // Err here, unlike a pre-receive "ng" report, and it must not be an unknown
            // outcome.
            let error = finished.expect_err("rejected push must return an error");
            let message = format!("{error:#}");
            assert!(
                message.contains("Git receive failed"),
                "{}: expected Git to refuse the transaction, got: {message}",
                case.name()
            );
            assert!(
                !error.is::<codeyard_nodes::writes::OutcomeUnknown>()
                    && !message.contains("outcome unknown"),
                "{}: a known pre-commit refusal must not report an unknown outcome: {message}",
                case.name()
            );
            assert_eq!(
                topic,
                Some(fixture.old),
                "{}: ref moved after authority was withdrawn",
                case.name()
            );
        }
        assert_eq!(
            bare.find_reference("refs/heads/main").unwrap().target(),
            Some(fixture.old),
            "{}",
            case.name()
        );
        // A rejected receive must release the interprocess lease and the process mutex even
        // though its post-receive hook never ran; an allowed one releases them in post-receive.
        timeout(
            Duration::from_secs(5),
            codeyard_git::locks::publication(&fixture.path),
        )
        .await
        .unwrap_or_else(|_| {
            panic!(
                "{}: publication lease was not released after the push",
                case.name()
            )
        })
        .unwrap();
        // Cleanup stays inside the test's bound; the pool also has its own close timeout.
        let _ = timeout(Duration::from_secs(10), fixture.pool.close()).await;
    }
}

/// 64 legal names at the ref-name limit, each under a distinct first component so their
/// expanded ancestor directories cannot be shared. The publication plan's byte budget refuses
/// this batch before admission.
fn deep_names() -> Vec<String> {
    (0..64)
        .map(|index| {
            let mut name = format!("refs/heads/{index:02x}/");
            while name.len() + "x/".len() + "y".len() <= MAX_REF_BYTES {
                name.push_str("x/");
            }
            name.push('y');
            name
        })
        .collect()
}

/// A batch that passes ref-name admission but cannot fit the barriered plan is refused at
/// pre-receive, before admitted names exist for the post-receive barrier. A pre-receive refusal
/// is an ordinary rejected report (Git exits 0), and Git must not have created the deep ref
/// lock ancestors.
async fn deep_plan(backend: Backend) {
    let mut fixture = Fixture::new(backend, 64).await;
    let names = deep_names();
    let oid = fixture.old.to_string();
    let proposed = names
        .iter()
        .map(|reference| codeyard_git::protocol::RefUpdate {
            old: ZERO_SHA.to_owned(),
            new: oid.clone(),
            reference: reference.clone(),
        })
        .collect::<Vec<_>>();
    codeyard_git::protocol::validate_updates(&proposed).unwrap();
    let updates = names
        .iter()
        .map(|name| (name.as_str(), ZERO_SHA, oid.as_str()))
        .collect::<Vec<_>>();
    fixture.request = receive_requests(&fixture.path, &updates);
    let mut task = fixture.receive();
    let finished = match timeout(Duration::from_secs(20), &mut task).await {
        Ok(joined) => joined.expect("receive task panicked"),
        Err(_) => {
            task.abort();
            let _ = timeout(Duration::from_secs(5), &mut task).await;
            panic!("deep-plan push did not finish");
        }
    };
    // A pre-receive refusal is in-band: receive-pack exits 0 with an "ng" report-status for
    // each ref, unlike the fatal prepared-hook case. A valid report is Ok here; an Err would
    // mean the broker lost the rejection or reported an unknown outcome for refs never written.
    let pushed = finished.expect("pre-receive refusal must be an ordinary rejected report");
    let output = String::from_utf8_lossy(&pushed);
    assert!(
        output.contains("ng refs/heads/") && !output.contains("ok refs/heads/"),
        "expected every deep ref rejected without success: {output}"
    );
    let bare = git2::Repository::open_bare(&fixture.path).unwrap();
    for name in &names {
        assert!(
            output.contains(&format!("ng {name} ")),
            "missing rejection: {name}"
        );
        assert!(bare.find_reference(name).is_err(), "{name}");
    }
    for index in 0..64u32 {
        assert!(
            !fixture
                .path
                .join(format!("refs/heads/{index:02x}"))
                .exists(),
            "refusal must happen before Git creates ref lock ancestors: {index:02x}"
        );
    }
    assert_eq!(
        bare.find_reference("refs/heads/main").unwrap().target(),
        Some(fixture.old)
    );
    timeout(
        Duration::from_secs(5),
        codeyard_git::locks::publication(&fixture.path),
    )
    .await
    .expect("publication lease was not released after the refusal")
    .unwrap();
    let _ = timeout(Duration::from_secs(10), fixture.pool.close()).await;
}

#[tokio::test]
async fn sqlite_unplannable_push_is_refused_before_admission() {
    deep_plan(Backend::Sqlite).await;
}

#[tokio::test]
async fn postgres_unplannable_push_is_refused_before_admission() {
    deep_plan(Backend::Postgres).await;
}

#[tokio::test]
async fn sqlite_prepared_publication_rechecks_live_authority() {
    contract(Backend::Sqlite).await;
}

#[tokio::test]
async fn postgres_prepared_publication_rechecks_live_authority() {
    contract(Backend::Postgres).await;
}
