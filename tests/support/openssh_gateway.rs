//! Gateway tests on throwaway databases and real Git repositories.
//! GITSERV_TEST_OPENSSH_HELPER must point at the built codeyard binary.
use super::*;
use crate::backend::{
    ci::{queue::RunQueue, CiWorkers},
    db::models::AccessLevel,
    notifications::NotificationBroadcaster,
};
use base64::Engine;
use std::os::unix::fs::PermissionsExt;
use std::process::{Output, Stdio};
use tokio::process::Command;
use tokio::time::{sleep, Duration};

#[path = "openssh_daemon.rs"]
mod daemon;

const ZERO_SHA: &str = "0000000000000000000000000000000000000000";

struct Database {
    pool: DbPool,
    native: sqlx::PgPool,
    schema: String,
    url: String,
}

impl Drop for Database {
    fn drop(&mut self) {
        let pool = self.pool.clone();
        let schema = self.schema.clone();
        let url = self.url.clone();
        let result = std::thread::spawn(move || {
            tokio::runtime::Runtime::new().unwrap().block_on(async {
                // This test's runtime is blocked on `join` below, so a background task that
                // holds a connection at this moment cannot return it. Do not wait for it:
                // dropping the schema is the cleanup that matters.
                let _ = tokio::time::timeout(Duration::from_secs(2), pool.close()).await;
                let admin = sqlx::postgres::PgPoolOptions::new()
                    .max_connections(1)
                    .connect(&url)
                    .await?;
                sqlx::raw_sql("SET lock_timeout = '5s'; SET statement_timeout = '10s'")
                    .execute(&admin)
                    .await?;
                sqlx::query("SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE application_name=$1 AND pid<>pg_backend_pid()")
                    .bind(&schema).execute(&admin).await?;
                let result = sqlx::query(&format!("DROP SCHEMA \"{schema}\" CASCADE"))
                    .execute(&admin)
                    .await;
                admin.close().await;
                result.map(|_| ()).map_err(anyhow::Error::from)
            })
        })
        .join();
        let failure = match result {
            Ok(Ok(())) => None,
            Ok(Err(error)) => Some(format!("{error:#}")),
            Err(_) => Some("cleanup thread panicked".to_string()),
        };
        if let Some(failure) = failure {
            if std::thread::panicking() {
                eprintln!("OpenSSH fixture schema cleanup failed: {failure}");
            } else {
                panic!("OpenSSH fixture schema cleanup failed: {failure}");
            }
        }
    }
}

async fn database() -> Database {
    let url = std::env::var("GITSERV_TEST_DATABASE_URL")
        .expect("required explicit disposable PostgreSQL URL");
    let parsed = url::Url::parse(&url).expect("test URL");
    assert!(
        matches!(parsed.scheme(), "postgres" | "postgresql")
            && parsed.path() == "/gitserv_test"
            && parsed.query().is_none()
            && parsed.fragment().is_none(),
        "only the owned gitserv_test database, without options, is permitted"
    );
    let schema = format!("openssh_{}", uuid::Uuid::new_v4().simple());
    let admin = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .expect("required disposable PostgreSQL is unavailable");
    sqlx::query(&format!("CREATE SCHEMA \"{schema}\""))
        .execute(&admin)
        .await
        .unwrap();
    let mut fixture = Database {
        pool: DbPool::from(admin.clone()),
        native: admin.clone(),
        schema,
        url,
    };
    admin.close().await;
    fixture.native = sqlx::postgres::PgPoolOptions::new()
        .max_connections(8)
        .connect(&format!(
            "{}?options=-csearch_path%3D{}%20-capplication_name%3D{}",
            fixture.url, fixture.schema, fixture.schema
        ))
        .await
        .unwrap();
    fixture.pool = DbPool::from(fixture.native.clone());
    db::run_migrations(&fixture.pool)
        .await
        .expect("required fixture migration");
    fixture
}

struct Broker {
    gateway: Arc<Gateway>,
    workers: CiWorkers,
    notifications: Arc<NotificationBroadcaster>,
    task: Option<tokio::task::JoinHandle<Result<()>>>,
    stop_listeners: tokio::sync::watch::Sender<bool>,
    directory: TempDir,
    helper: PathBuf,
}

impl Drop for Broker {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

impl Broker {
    async fn new(pool: DbPool) -> Self {
        Self::new_for_user(pool, "git".to_owned()).await
    }

    async fn new_for_user(pool: DbPool, git_user: String) -> Self {
        Self::new_with_ci(pool, git_user, None).await
    }

    async fn new_with_ci(
        pool: DbPool,
        git_user: String,
        config: Option<crate::backend::ci::CiConfig>,
    ) -> Self {
        let uid = private_files::effective_uid().unwrap();
        let helper = PathBuf::from(
            std::env::var("GITSERV_TEST_OPENSSH_HELPER")
                .expect("set GITSERV_TEST_OPENSSH_HELPER to the integrated application binary"),
        );
        let helper =
            private_files::trusted_executable(&std::fs::canonicalize(helper).unwrap(), uid)
                .unwrap();
        let directory = private_files::private_tempdir("openssh-broker-").unwrap();
        let root = directory.path().join("repos");
        std::fs::create_dir(&root).unwrap();
        let socket = directory.path().join("broker.sock");
        let (listener, guard) = private_files::bind(&socket, uid).await.unwrap();
        let run_workers = config.is_some();
        let config = Arc::new(config.unwrap_or_default());
        let queued = Arc::new(tokio::sync::Notify::new());
        let notifications = Arc::new(NotificationBroadcaster::new(&pool));
        notifications.start().await;
        let ci = CiRuns::new(
            pool.clone(),
            config.clone(),
            queued.clone(),
            None,
            notifications.clone(),
        );
        let workers = CiWorkers::with_wakeup(
            RunQueue::Database {
                pool: pool.clone(),
                config: config.clone(),
                secrets: None,
                context: None,
                notifications: notifications.clone(),
            },
            config,
            run_workers,
            queued,
            directory.path(),
        );
        workers.startup().await.expect("start CI workers");
        let gateway = Arc::new(Gateway {
            socket,
            repos_root: root,
            executable: helper.clone(),
            git: private_files::git_executable(uid).unwrap(),
            pool: pool.clone(),
            ci: Arc::new(ci),
            git_user,
            uid,
            sessions: Arc::new(Semaphore::new(MAX_SESSIONS)),
            controls: Semaphore::new(MAX_CONTROLS),
            operations: Mutex::new(HashMap::new()),
        });
        let (stop_listeners, stopped) = tokio::sync::watch::channel(false);
        let server = gateway.clone();
        let task = tokio::spawn(async move {
            let _guard = guard;
            listen(server, listener, stopped).await
        });
        Self {
            gateway,
            workers,
            notifications,
            task: Some(task),
            stop_listeners,
            directory,
            helper,
        }
    }

    async fn stop(mut self) {
        let task = self.task.take().unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(!self.gateway.socket.exists(), "owned socket cleanup");
        self.workers
            .shutdown()
            .await
            .expect("drain owned CI fixture");
        self.notifications.shutdown().await;
    }

    fn wrapper(&self, key_id: i64) -> PathBuf {
        let wrapper = self.directory.path().join(format!("ssh-{key_id}"));
        // Git's SSH command is the last argument. This wrapper doesn't interpret
        // it as shell source, and calls the actual CLI helper in another process.
        std::fs::write(&wrapper, format!("#!/bin/sh\nfor argument do command=$argument; done\nexport SSH_ORIGINAL_COMMAND=\"$command\"\nexec {} ssh serve --socket {} --key-id {}\n", self.helper.display(), self.gateway.socket.display(), key_id)).unwrap();
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
        wrapper
    }

    fn key_helper(&self) -> PathBuf {
        let helper = self.directory.path().join("serve-helper");
        std::fs::write(
            &helper,
            format!(
                "#!/bin/sh\nexec {} ssh serve --socket {} --key-id \"$1\"\n",
                self.helper.display(),
                self.gateway.socket.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o700)).unwrap();
        helper
    }

    async fn keys(&self, key: &str, user: &str) -> Output {
        let mut fields = key.split_whitespace();
        let kind = fields.next().unwrap();
        let blob = fields.next().unwrap();
        let mut process = Command::new(&self.helper);
        process
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env(
                "DATABASE_URL",
                "invalid-no-helper-must-initialize-a-database",
            )
            .args(["ssh", "keys", "--socket"])
            .arg(&self.gateway.socket)
            .args([
                "--expected-user",
                self.gateway.git_user.as_str(),
                "--serve-command",
            ])
            .arg(self.key_helper())
            .args([user, kind, blob]);
        timeout(Duration::from_secs(15), process.kill_on_drop(true).output())
            .await
            .unwrap()
            .unwrap()
    }

    async fn serve(&self, key_id: i64, command: &str) -> Output {
        timeout(
            Duration::from_secs(15),
            Command::new(&self.helper)
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .env("SSH_ORIGINAL_COMMAND", command)
                .env("GIT_CONFIG_COUNT", "1")
                .env("GIT_CONFIG_KEY_0", "core.hooksPath")
                .env("GIT_CONFIG_VALUE_0", "/evil")
                .args(["ssh", "serve", "--socket"])
                .arg(&self.gateway.socket)
                .arg("--key-id")
                .arg(key_id.to_string())
                .stdin(Stdio::null())
                .kill_on_drop(true)
                .output(),
        )
        .await
        .unwrap()
        .unwrap()
    }

    async fn git(&self, directory: &Path, key_id: i64, arguments: &[&str]) -> Output {
        timeout(
            Duration::from_secs(30),
            Command::new(&self.gateway.git)
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .env("HOME", self.directory.path())
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_SSH_COMMAND", self.wrapper(key_id))
                .env("GIT_SSH_VARIANT", "ssh")
                .env("GIT_TERMINAL_PROMPT", "0")
                .env("LC_ALL", "C")
                .current_dir(directory)
                .args(arguments)
                .kill_on_drop(true)
                .output(),
        )
        .await
        .unwrap()
        .unwrap()
    }

    async fn no_hooks_left(&self) {
        for _ in 0..100 {
            let count = std::fs::read_dir(self.directory.path())
                .unwrap()
                .filter_map(std::result::Result::ok)
                .filter(|entry| {
                    entry
                        .file_name()
                        .to_string_lossy()
                        .starts_with("receive-hooks-")
                })
                .count();
            if count == 0 && self.gateway.operations.lock().unwrap().is_empty() {
                return;
            }
            sleep(Duration::from_millis(10)).await;
        }
        panic!("receive operation/hook state leaked");
    }
}

fn key(byte: u8) -> String {
    let mut wire = Vec::from(&b"\0\0\0\x0bssh-ed25519\0\0\0\x20"[..]);
    wire.extend_from_slice(&[byte; 32]);
    format!(
        "ssh-ed25519 {}",
        base64::engine::general_purpose::STANDARD.encode(wire)
    )
}

fn commit(repository: &git2::Repository, contents: &str) -> git2::Oid {
    let root = repository.workdir().unwrap();
    std::fs::write(root.join("README"), contents).unwrap();
    let mut index = repository.index().unwrap();
    index.add_path(Path::new("README")).unwrap();
    index.write().unwrap();
    let tree = repository.find_tree(index.write_tree().unwrap()).unwrap();
    let signature = git2::Signature::now("Fixture", "fixture@example.invalid").unwrap();
    let parent = repository
        .head()
        .ok()
        .and_then(|head| head.peel_to_commit().ok());
    let parents: Vec<_> = parent.iter().collect();
    repository
        .commit(
            Some("HEAD"),
            &signature,
            &signature,
            "fixture change",
            &tree,
            &parents,
        )
        .unwrap()
}

/// A new Git session during shutdown is refused: Git never runs, and the broker says why.
fn assert_refused(output: &Output) {
    assert!(output.stdout.is_empty(), "refused session reached Git");
    assert!(!output.status.success(), "refused session succeeded");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("OpenSSH Git operation denied or failed."),
        "unexpected refusal: {stderr}"
    );
}

#[tokio::test]
async fn real_keys_private_clone_streamed_fetch_push_and_protected_multi_ref() {
    let database = database().await;
    let owner = db::users::accounts::create_user(&database.pool, "alice", None)
        .await
        .unwrap();
    let reader = db::users::accounts::create_user(&database.pool, "reader", None)
        .await
        .unwrap();
    // Only trusted actors run workflows, so the write collaborator is one: it still
    // cannot admit a deployment workflow.
    let broker = Broker::new_with_ci(
        database.pool.clone(),
        "git".to_owned(),
        Some(crate::backend::ci::CiConfig {
            trusted_actor_ids: [reader.id].into(),
            ..Default::default()
        }),
    )
    .await;
    let owner_key = db::users::ssh_keys::add_ssh_key(&database.pool, owner.id, "owner", &key(7))
        .await
        .unwrap();
    let read_key = db::users::ssh_keys::add_ssh_key(&database.pool, reader.id, "reader", &key(8))
        .await
        .unwrap();
    let bare_path = broker.gateway.repos_root.join("private.git");
    let mut options = git2::RepositoryInitOptions::new();
    options.bare(true).initial_head("main");
    let bare = git2::Repository::init_opts(&bare_path, &options).unwrap();
    let repo = db::repositories::records::create_repository(
        &database.pool,
        owner.id,
        "private",
        None,
        true,
        bare_path.to_str().unwrap(),
    )
    .await
    .unwrap();
    // Repo-owned hooks must never execute. A blocking repo pre-hook would make
    // the successful push below fail if core.hooksPath were not overridden.
    std::fs::write(bare_path.join("hooks/pre-receive"), b"#!/bin/sh\nexit 99\n").unwrap();
    std::fs::set_permissions(
        bare_path.join("hooks/pre-receive"),
        std::fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    let marker = broker.directory.path().join("repo-hook-ran");
    std::fs::write(
        bare_path.join("hooks/post-receive"),
        format!("#!/bin/sh\ntouch {}\n", marker.display()),
    )
    .unwrap();
    std::fs::set_permissions(
        bare_path.join("hooks/post-receive"),
        std::fs::Permissions::from_mode(0o700),
    )
    .unwrap();

    // Git ignores this protected-config-only option in repository config.
    // The broker must not promote an empty override into protected config.
    bare.config()
        .unwrap()
        .set_str(
            "uploadpack.packObjectsHook",
            bare_path.join("hooks/pre-receive").to_str().unwrap(),
        )
        .unwrap();

    let acceptance = broker.keys(&key(7), "git").await;
    assert!(acceptance.status.success());
    assert_eq!(
        String::from_utf8(acceptance.stdout).unwrap(),
        format!(
            "restrict,command=\"{} {}\" {}\n",
            broker.key_helper().display(),
            owner_key.id,
            key(7)
        )
    );
    assert!(broker.keys(&key(9), "git").await.stdout.is_empty());
    assert!(broker.keys(&key(7), "not-git").await.stdout.is_empty());
    let mut slots = Vec::new();
    for _ in 0..MAX_SESSIONS {
        slots.push(broker.gateway.sessions.try_acquire().unwrap());
    }
    assert!(
        !broker.keys(&key(7), "git").await.stdout.is_empty(),
        "keys are not gated by Git concurrency"
    );
    drop(slots);

    let source_path = broker.directory.path().join("source");
    options.bare(false);
    let source = git2::Repository::init_opts(&source_path, &options).unwrap();
    let first = commit(&source, "first\n");
    let push = broker
        .git(
            &source_path,
            owner_key.id,
            &[
                "push",
                "git@fixture:alice/private.git",
                "HEAD:refs/heads/main",
            ],
        )
        .await;
    assert!(
        push.status.success(),
        "initial streamed push: {}",
        String::from_utf8_lossy(&push.stderr)
    );
    assert_eq!(
        bare.find_reference("refs/heads/main").unwrap().target(),
        Some(first)
    );
    assert!(!marker.exists());
    broker.no_hooks_left().await;

    let denied_clone = broker
        .git(
            broker.directory.path(),
            read_key.id,
            &["clone", "git@fixture:alice/private.git", "denied-clone"],
        )
        .await;
    assert!(!denied_clone.status.success());
    db::repositories::access::grant_repo_access(
        &database.pool,
        reader.id,
        repo.id,
        AccessLevel::Read,
    )
    .await
    .unwrap();
    let clone = broker
        .git(
            broker.directory.path(),
            read_key.id,
            &["clone", "git@fixture:alice/private.git", "clone"],
        )
        .await;
    assert!(
        clone.status.success(),
        "private streamed clone: {}",
        String::from_utf8_lossy(&clone.stderr)
    );
    assert_eq!(
        git2::Repository::open(broker.directory.path().join("clone"))
            .unwrap()
            .head()
            .unwrap()
            .target(),
        Some(first)
    );
    let second = commit(&source, "second\n");
    assert!(!broker
        .git(
            &source_path,
            read_key.id,
            &[
                "push",
                "git@fixture:alice/private.git",
                "HEAD:refs/heads/topic"
            ]
        )
        .await
        .status
        .success());
    assert!(bare.find_reference("refs/heads/topic").is_err());
    db::repositories::access::grant_repo_access(
        &database.pool,
        reader.id,
        repo.id,
        AccessLevel::Write,
    )
    .await
    .unwrap();
    let push = broker
        .git(
            &source_path,
            read_key.id,
            &[
                "push",
                "git@fixture:alice/private.git",
                "HEAD:refs/heads/topic",
            ],
        )
        .await;
    assert!(
        push.status.success(),
        "write collaborator push: {}",
        String::from_utf8_lossy(&push.stderr)
    );
    assert_eq!(
        bare.find_reference("refs/heads/topic").unwrap().target(),
        Some(second)
    );
    let fetch = broker
        .git(
            &broker.directory.path().join("clone"),
            read_key.id,
            &["fetch", "origin"],
        )
        .await;
    assert!(fetch.status.success(), "real streamed fetch");

    // A write collaborator may push a deployment workflow, but cannot admit
    // CD/full. Git writes stay accepted and the dispatch failure is explicit.
    std::fs::create_dir_all(source_path.join(".github/workflows")).unwrap();
    std::fs::write(
        source_path.join(".github/workflows/deploy.yml"),
        "name: deploy\non: push\njobs: {}\n",
    )
    .unwrap();
    let mut index = source.index().unwrap();
    index
        .add_path(Path::new(".github/workflows/deploy.yml"))
        .unwrap();
    index.write().unwrap();
    let deployment = commit(&source, "deployment definition\n");
    let result = broker
        .git(
            &source_path,
            read_key.id,
            &[
                "push",
                "git@fixture:alice/private.git",
                "HEAD:refs/heads/deploy",
            ],
        )
        .await;
    assert_eq!(
        bare.find_reference("refs/heads/deploy").unwrap().target(),
        Some(deployment)
    );
    assert!(
        String::from_utf8_lossy(&result.stderr).contains("accepted"),
        "accepted writes must not be presented as undone: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    let runs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM ci_runs WHERE repo_id = $1")
        .bind(repo.id)
        .fetch_one(&database.native)
        .await
        .unwrap();
    assert_eq!(
        runs, 0,
        "automatic push must not bypass current admin/owner access for CD"
    );

    // An out-of-operation post-receive request cannot claim accepted updates.
    let mut connection = private_files::connect(&broker.gateway.socket)
        .await
        .unwrap();
    protocol::write_request(
        &mut connection,
        &Request::Hook {
            key_id: owner_key.id,
            repo_id: repo.id,
            token: uuid::Uuid::new_v4().simple().to_string(),
            post: true,
            updates: vec![RefUpdate {
                old: ZERO_SHA.to_owned(),
                new: deployment.to_string(),
                reference: "refs/heads/deploy".to_owned(),
            }],
            quarantine: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(
        protocol::read_frame(&mut connection).await.unwrap().0,
        STDERR
    );
    assert_eq!(
        protocol::read_frame(&mut connection).await.unwrap(),
        (protocol::EXIT, 1i32.to_be_bytes().to_vec())
    );

    db::repositories::protection::upsert_branch_protection_rule(
        &database.pool,
        db::repositories::protection::BranchProtectionRuleUpsert {
            actor_id: owner.id,
            repo_id: repo.id,
            pattern: "main",
            require_pull_request: true,
            required_approving_reviews: 0,
            dismiss_stale_reviews: false,
            require_status_checks: false,
            required_status_checks: vec![],
            require_up_to_date_branch: false,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let rejected = broker
        .git(
            &source_path,
            owner_key.id,
            &[
                "push",
                "git@fixture:alice/private.git",
                "HEAD:refs/heads/main",
                "HEAD:refs/heads/unprotected",
            ],
        )
        .await;
    assert!(!rejected.status.success());
    assert_eq!(
        bare.find_reference("refs/heads/main").unwrap().target(),
        Some(first)
    );
    assert!(
        bare.find_reference("refs/heads/unprotected").is_err(),
        "multi-ref pre-hook fails the whole push"
    );
    assert!(!broker
        .git(
            &source_path,
            owner_key.id,
            &["push", "git@fixture:alice/private.git", ":refs/heads/main"]
        )
        .await
        .status
        .success());
    broker.no_hooks_left().await;
    let runs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM ci_runs WHERE repo_id = $1")
        .bind(repo.id)
        .fetch_one(&database.native)
        .await
        .unwrap();
    assert_eq!(
        runs, 0,
        "proposed/failed and forged updates must never trigger CI"
    );

    sqlx::query("UPDATE repositories SET is_archived = TRUE WHERE id = $1")
        .bind(repo.id)
        .execute(&database.native)
        .await
        .unwrap();
    assert!(!broker
        .git(
            &source_path,
            owner_key.id,
            &[
                "push",
                "git@fixture:alice/private.git",
                "HEAD:refs/heads/archived"
            ]
        )
        .await
        .status
        .success());
    assert!(bare.find_reference("refs/heads/archived").is_err());
    assert_eq!(
        db::repositories::records::get_repository_by_id(&database.pool, repo.id)
            .await
            .unwrap()
            .unwrap()
            .default_branch,
        "main"
    );
    broker.stop().await;
}

#[tokio::test]
async fn helper_disconnect_releases_shared_git_session_during_negotiation_and_incomplete_receive_pack(
) {
    let database = database().await;
    let broker = Broker::new(database.pool.clone()).await;
    let user = db::users::accounts::create_user(&database.pool, "disconnect-owner", None)
        .await
        .unwrap();
    let key =
        db::users::ssh_keys::add_ssh_key(&database.pool, user.id, "disconnect fixture", &key(41))
            .await
            .unwrap();
    let bare_path = broker.gateway.repos_root.join("disconnect.git");
    git2::Repository::init_bare(&bare_path).unwrap();
    db::repositories::records::create_repository(
        &database.pool,
        user.id,
        "disconnect",
        None,
        true,
        bare_path.to_str().unwrap(),
    )
    .await
    .unwrap();

    // Leave one shared slot for the broker session under test.
    let held: Vec<_> = (1..MAX_SESSIONS)
        .map(|_| broker.gateway.sessions.try_acquire().unwrap())
        .collect();
    for (command, incomplete_input) in [
        ("git-upload-pack 'disconnect-owner/disconnect.git'", None),
        (
            "git-receive-pack 'disconnect-owner/disconnect.git'",
            Some(b"0040".as_slice()),
        ),
    ] {
        let mut connection = private_files::connect(&broker.gateway.socket)
            .await
            .unwrap();
        protocol::write_request(
            &mut connection,
            &Request::Serve {
                key_id: key.id,
                command: command.to_owned(),
                git_protocol: None,
            },
        )
        .await
        .unwrap();
        let (tag, advertisement) = timeout(
            Duration::from_secs(5),
            protocol::read_frame(&mut connection),
        )
        .await
        .expect("Git advertisement timeout")
        .unwrap();
        assert_eq!(tag, STDOUT, "unexpected broker frame: {advertisement:?}");
        assert!(!advertisement.is_empty());
        assert_eq!(broker.gateway.sessions.available_permits(), 0);
        if let Some(bytes) = incomplete_input {
            protocol::write_frame(&mut connection, STDIN, bytes)
                .await
                .unwrap();
        }

        // Dropping without the explicit EOF frame is the helper-disconnect boundary.
        drop(connection);
        let released = timeout(Duration::from_secs(3), broker.gateway.sessions.acquire())
            .await
            .expect("disconnected Git session retained its permit")
            .unwrap();
        assert!(broker.gateway.operations.lock().unwrap().is_empty());
        drop(released);

        let admitted = timeout(
            Duration::from_secs(5),
            broker.serve(key.id, "git-upload-pack 'disconnect-owner/disconnect.git'"),
        )
        .await
        .expect("next shared Git session was not admitted promptly");
        assert!(
            !admitted.stdout.is_empty(),
            "next session did not reach Git: {}",
            String::from_utf8_lossy(&admitted.stderr)
        );
    }
    drop(held);
    broker.no_hooks_left().await;
    broker.stop().await;
}

#[tokio::test]
async fn stop_finishes_open_connections_and_refuses_new_ones() {
    let database = database().await;
    let mut broker = Broker::new(database.pool.clone()).await;
    let user = db::users::accounts::create_user(&database.pool, "drain-owner", None)
        .await
        .unwrap();
    let key = db::users::ssh_keys::add_ssh_key(&database.pool, user.id, "drain", &key(43))
        .await
        .unwrap();
    let bare_path = broker.gateway.repos_root.join("drain.git");
    git2::Repository::init_bare(&bare_path).unwrap();
    db::repositories::records::create_repository(
        &database.pool,
        user.id,
        "drain",
        None,
        true,
        bare_path.to_str().unwrap(),
    )
    .await
    .unwrap();

    let mut open = private_files::connect(&broker.gateway.socket)
        .await
        .unwrap();
    protocol::write_request(
        &mut open,
        &Request::Serve {
            key_id: key.id,
            command: "git-upload-pack 'drain-owner/drain.git'".to_owned(),
            git_protocol: None,
        },
    )
    .await
    .unwrap();
    // The advertisement shows that the broker accepted and is serving this connection.
    let (tag, advertisement) = timeout(Duration::from_secs(5), protocol::read_frame(&mut open))
        .await
        .expect("Git advertisement timeout")
        .unwrap();
    assert_eq!(tag, STDOUT);
    assert!(!advertisement.is_empty());

    broker.stop_listeners.send_replace(true);
    // The open session keeps the listener up, so a new session is refused, not unreachable.
    assert_refused(
        &broker
            .serve(key.id, "git-upload-pack 'drain-owner/drain.git'")
            .await,
    );

    // The open session is not cancelled: its client closes stdin, and the session still
    // ends with its exit frame. Git's own status is not what this test is about.
    protocol::write_frame(&mut open, EOF, b"").await.unwrap();
    timeout(Duration::from_secs(10), async {
        loop {
            let (tag, _) = protocol::read_frame(&mut open).await.unwrap();
            if tag == protocol::EXIT {
                break;
            }
        }
    })
    .await
    .expect("open session did not finish after stop");

    // The listener task ends once its last connection has finished, and removes the socket.
    let task = broker.task.take().unwrap();
    timeout(Duration::from_secs(5), task)
        .await
        .expect("gateway did not finish draining")
        .unwrap()
        .unwrap();
    assert!(!broker.gateway.socket.exists());
    broker.workers.shutdown().await.unwrap();
    broker.notifications.shutdown().await;
}

#[tokio::test]
async fn stop_lets_a_running_push_finish_its_hooks() {
    let database = database().await;
    let mut broker = Broker::new(database.pool.clone()).await;
    let user = db::users::accounts::create_user(&database.pool, "drain-owner", None)
        .await
        .unwrap();
    let key = db::users::ssh_keys::add_ssh_key(&database.pool, user.id, "drain", &key(43))
        .await
        .unwrap();
    let bare_path = broker.gateway.repos_root.join("drain.git");
    git2::Repository::init_bare(&bare_path).unwrap();
    let repo = db::repositories::records::create_repository(
        &database.pool,
        user.id,
        "drain",
        None,
        true,
        bare_path.to_str().unwrap(),
    )
    .await
    .unwrap();
    // The push is held at its pre-receive hook, after Git has received the pack.
    let (reached, release) =
        policy_tests::hold_next_hook(std::fs::canonicalize(&bare_path).unwrap(), false);

    let source_path = broker.directory.path().join("source");
    let source = git2::Repository::init(&source_path).unwrap();
    commit(&source, "drain\n");
    let push = broker.git(
        &source_path,
        key.id,
        &[
            "push",
            "git@fixture:drain-owner/drain.git",
            "HEAD:refs/heads/main",
        ],
    );
    let shutdown = async {
        timeout(Duration::from_secs(15), reached)
            .await
            .expect("push never reached its pre-receive hook")
            .unwrap();
        broker.stop_listeners.send_replace(true);
        // The held push keeps the listener up, so a new session is refused.
        let refused = broker
            .serve(key.id, "git-upload-pack 'drain-owner/drain.git'")
            .await;
        release.send(()).unwrap();
        refused
    };
    let (push, refused) = tokio::join!(push, shutdown);
    assert!(
        push.status.success(),
        "push during shutdown: {}",
        String::from_utf8_lossy(&push.stderr)
    );
    // Post-receive ran during the drain, so the stored size reflects the pushed objects.
    let stored = db::repositories::records::get_repository_by_id(&database.pool, repo.id)
        .await
        .unwrap()
        .unwrap();
    assert!(stored.size_bytes > 0, "post-receive metadata did not run");
    assert_refused(&refused);
    broker.no_hooks_left().await;
    let task = broker.task.take().unwrap();
    timeout(Duration::from_secs(5), task)
        .await
        .expect("gateway did not finish draining")
        .unwrap()
        .unwrap();
    assert!(!broker.gateway.socket.exists());
    broker.workers.shutdown().await.unwrap();
    broker.notifications.shutdown().await;
}

#[tokio::test]
async fn stop_finishes_a_push_after_its_helper_disconnects() {
    use std::io::Write;
    let database = database().await;
    let mut broker = Broker::new(database.pool.clone()).await;
    let user = db::users::accounts::create_user(&database.pool, "drain-owner", None)
        .await
        .unwrap();
    let key = db::users::ssh_keys::add_ssh_key(&database.pool, user.id, "drain", &key(43))
        .await
        .unwrap();
    let bare_path = broker.gateway.repos_root.join("drain.git");
    git2::Repository::init_bare(&bare_path).unwrap();
    let repo = db::repositories::records::create_repository(
        &database.pool,
        user.id,
        "drain",
        None,
        true,
        bare_path.to_str().unwrap(),
    )
    .await
    .unwrap();
    let source_path = broker.directory.path().join("source");
    let source = git2::Repository::init(&source_path).unwrap();
    let oid = commit(&source, "drain\n");
    // The complete pack for the new branch, as a Git client sends it after the commands.
    let mut pack = std::process::Command::new("git")
        .arg("-C")
        .arg(&source_path)
        .args(["pack-objects", "--stdout", "--revs"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = pack.stdin.take().unwrap();
    writeln!(input, "{oid}").unwrap();
    drop(input);
    let output = pack.wait_with_output().unwrap();
    assert!(output.status.success());
    let command = format!("{ZERO_SHA} {oid} refs/heads/main\0report-status\n");
    let mut request = format!("{:04x}{command}0000", command.len() + 4).into_bytes();
    request.extend_from_slice(&output.stdout);

    // The push is held at its pre-receive hook, after Git has the complete pack.
    let (reached, release) =
        policy_tests::hold_next_hook(std::fs::canonicalize(&bare_path).unwrap(), false);
    let mut connection = private_files::connect(&broker.gateway.socket)
        .await
        .unwrap();
    protocol::write_request(
        &mut connection,
        &Request::Serve {
            key_id: key.id,
            command: "git-receive-pack 'drain-owner/drain.git'".to_owned(),
            git_protocol: None,
        },
    )
    .await
    .unwrap();
    let (tag, advertisement) = timeout(
        Duration::from_secs(5),
        protocol::read_frame(&mut connection),
    )
    .await
    .expect("Git advertisement timeout")
    .unwrap();
    assert_eq!(tag, STDOUT);
    assert!(!advertisement.is_empty());
    protocol::write_frame(&mut connection, STDIN, &request)
        .await
        .unwrap();
    protocol::write_frame(&mut connection, EOF, b"")
        .await
        .unwrap();
    timeout(Duration::from_secs(15), reached)
        .await
        .expect("push never reached its pre-receive hook")
        .unwrap();

    // The helper disconnects before Git reports. Shutdown then starts while the push is held.
    drop(connection);
    broker.stop_listeners.send_replace(true);
    release.send(()).unwrap();
    let task = broker.task.take().unwrap();
    timeout(Duration::from_secs(15), task)
        .await
        .expect("gateway did not finish draining")
        .unwrap()
        .unwrap();
    let bare = git2::Repository::open_bare(&bare_path).unwrap();
    assert_eq!(
        bare.find_reference("refs/heads/main").unwrap().target(),
        Some(oid)
    );
    // Post-receive ran during the drain, so the stored size reflects the pushed objects.
    let stored = db::repositories::records::get_repository_by_id(&database.pool, repo.id)
        .await
        .unwrap()
        .unwrap();
    assert!(stored.size_bytes > 0, "post-receive metadata did not run");
    broker.no_hooks_left().await;
    assert!(!broker.gateway.socket.exists());
    broker.workers.shutdown().await.unwrap();
    broker.notifications.shutdown().await;
}

struct PushFixture {
    directory: TempDir,
    state: crate::backend::app::AppState,
    repo: Repository,
    token: String,
    oid: git2::Oid,
    pack: Vec<u8>,
}

impl PushFixture {
    async fn new(pool: DbPool) -> Self {
        let directory = tempfile::tempdir().unwrap();
        db::node_config::upsert_node_configuration(&pool, "public", true)
            .await
            .unwrap();
        let owner = db::users::accounts::create_user(&pool, "push-owner", None)
            .await
            .unwrap();
        // Git sessions find a repository by the UUID in its directory name.
        let path = directory
            .path()
            .join(format!("{}.git", uuid::Uuid::new_v4()));
        git2::Repository::init_bare(&path)
            .unwrap()
            .set_head("refs/heads/main")
            .unwrap();
        let repo = db::repositories::records::create_repository(
            &pool,
            owner.id,
            "repo",
            None,
            true,
            path.to_str().unwrap(),
        )
        .await
        .unwrap();
        let session = uuid::Uuid::new_v4().simple().to_string().repeat(2);
        db::users::sessions::create_session(
            &pool,
            &session,
            owner.id,
            None,
            None,
            chrono::Utc::now() + chrono::Duration::hours(1),
        )
        .await
        .unwrap();
        let token = uuid::Uuid::new_v4().simple().to_string().repeat(2);
        db::users::tokens::create_token(
            &pool,
            owner.id,
            &session,
            "push",
            &crate::backend::identity::tokens::token_hash(&token),
            "write",
            None,
        )
        .await
        .unwrap()
        .unwrap();
        let source = git2::Repository::init(directory.path().join("source")).unwrap();
        let oid = commit(&source, "push\n");
        let mut builder = source.packbuilder().unwrap();
        builder.insert_commit(oid).unwrap();
        let mut pack = git2::Buf::new();
        builder.write_buf(&mut pack).unwrap();
        let storage = Arc::new(
            crate::backend::storage::ObjectStore::open(directory.path(), None)
                .await
                .unwrap(),
        );
        let state = crate::backend::app::AppState::new(
            pool,
            directory.path().to_owned(),
            storage,
            None,
            crate::backend::ci::CiConfig::default(),
            None,
        );
        Self {
            directory,
            state,
            repo,
            token,
            oid,
            pack: pack.to_vec(),
        }
    }

    fn request(&self, branch: &str) -> Vec<u8> {
        let command = format!(
            "{ZERO_SHA} {} refs/heads/{branch}\0report-status\n",
            self.oid
        );
        let mut request = format!("{:04x}{command}0000", command.len() + 4).into_bytes();
        request.extend_from_slice(&self.pack);
        request
    }

    async fn start(
        &self,
        ssh: bool,
    ) -> (
        std::net::SocketAddr,
        tokio::sync::watch::Sender<bool>,
        tokio::task::JoinHandle<Result<()>>,
    ) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let (stop, stopped) = tokio::sync::watch::channel(false);
        let state = self.state.clone();
        let mut server = if ssh {
            Some(
                crate::backend::ssh::SshServer::new(
                    state.pool.clone(),
                    None,
                    state.ci_runs.clone(),
                )
                .await
                .unwrap(),
            )
        } else {
            None
        };
        let running = tokio::spawn(async move {
            if let Some(server) = server.as_mut() {
                server.run(address, stopped).await
            } else {
                crate::backend::api::run_web_server(address, state, stopped).await
            }
        });
        timeout(Duration::from_secs(5), async {
            loop {
                if tokio::net::TcpStream::connect(address).await.is_ok() {
                    break;
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        (address, stop, running)
    }

    async fn http_push(
        &self,
        address: std::net::SocketAddr,
        branch: &str,
    ) -> tokio::net::TcpStream {
        let request = self.request(branch);
        let auth = base64::engine::general_purpose::STANDARD.encode(format!("git:{}", self.token));
        let headers = format!(
            "POST /push-owner/repo.git/git-receive-pack HTTP/1.1\r\nHost: {address}\r\nAuthorization: Basic {auth}\r\nContent-Type: application/x-git-receive-pack-request\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            request.len(),
        );
        let mut connection = tokio::net::TcpStream::connect(address).await.unwrap();
        connection.write_all(headers.as_bytes()).await.unwrap();
        connection.write_all(&request).await.unwrap();
        connection
    }

    async fn ssh_push(&self, address: std::net::SocketAddr) -> tokio::process::Child {
        let key = self.directory.path().join("key");
        assert!(Command::new(daemon::tool("ssh-keygen"))
            .args(["-q", "-t", "ed25519", "-N", "", "-f"])
            .arg(&key)
            .output()
            .await
            .unwrap()
            .status
            .success());
        db::users::ssh_keys::add_ssh_key(
            &self.state.pool,
            self.repo.personal_owner_id().unwrap(),
            "push",
            &std::fs::read_to_string(key.with_extension("pub")).unwrap(),
        )
        .await
        .unwrap();
        let mut child = Command::new(daemon::tool("ssh"))
            .args([
                "-F",
                "/dev/null",
                "-o",
                "StrictHostKeyChecking=no",
                "-o",
                "UserKnownHostsFile=/dev/null",
                "-o",
                "IdentitiesOnly=yes",
                "-o",
                "BatchMode=yes",
                "-i",
            ])
            .arg(key)
            .arg("-p")
            .arg(address.port().to_string())
            .args(["git@127.0.0.1", "git-receive-pack 'push-owner/repo.git'"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        // Read the advertisement before sending a complete receive-pack request.
        let output = child.stdout.as_mut().unwrap();
        timeout(Duration::from_secs(10), async {
            loop {
                let mut length = [0; 4];
                output.read_exact(&mut length).await.unwrap();
                let length =
                    usize::from_str_radix(std::str::from_utf8(&length).unwrap(), 16).unwrap();
                if length == 0 {
                    break;
                }
                let mut bytes = vec![0; length - 4];
                output.read_exact(&mut bytes).await.unwrap();
            }
        })
        .await
        .unwrap();
        let mut input = child.stdin.take().unwrap();
        input.write_all(&self.request("trunk")).await.unwrap();
        input.shutdown().await.unwrap();
        child
    }

    async fn assert_metadata(&self, branch: &str) {
        let stored =
            db::repositories::records::get_repository_by_id(&self.state.pool, self.repo.id)
                .await
                .unwrap()
                .unwrap();
        assert_eq!(stored.default_branch, branch);
        assert!(
            stored.size_bytes > 0,
            "post-receive metadata did not finish"
        );
        let bare = git2::Repository::open_bare(&self.repo.disk_path).unwrap();
        assert_eq!(
            bare.head().unwrap().name(),
            Some(format!("refs/heads/{branch}").as_str())
        );
        assert_eq!(bare.head().unwrap().target(), Some(self.oid));
    }
}

async fn disconnected_push_drains(pool: DbPool, ssh: bool) {
    let fixture = PushFixture::new(pool).await;
    let (address, stop, mut running) = fixture.start(ssh).await;
    let (reached, release) = policy_tests::hold_next_hook(
        std::fs::canonicalize(&fixture.repo.disk_path).unwrap(),
        true,
    );
    let mut http = None;
    let mut client = None;
    if ssh {
        client = Some(fixture.ssh_push(address).await);
    } else {
        http = Some(fixture.http_push(address, "trunk").await);
    }
    timeout(Duration::from_secs(15), reached)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        git2::Repository::open_bare(&fixture.repo.disk_path)
            .unwrap()
            .find_reference("refs/heads/trunk")
            .unwrap()
            .target(),
        Some(fixture.oid)
    );
    drop(http);
    if let Some(mut client) = client {
        client.kill().await.unwrap();
        client.wait().await.unwrap();
    }
    stop.send_replace(true);
    let early = timeout(Duration::from_millis(300), &mut running).await;
    // Release before asserting so a failing regression does not leave a hook behind.
    release.send(()).unwrap();
    assert!(
        early.is_err(),
        "server returned with disconnected post-receive work still pending"
    );
    timeout(Duration::from_secs(15), running)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    fixture.assert_metadata("trunk").await;
}

async fn concurrent_first_pushes(pool: DbPool) {
    let fixture = PushFixture::new(pool.clone()).await;
    let (address, stop, running) = fixture.start(false).await;
    let path = std::fs::canonicalize(&fixture.repo.disk_path).unwrap();
    let lock = crate::backend::git::locks::lock(&path).await;
    let (first_reached, first_release) = policy_tests::hold_next_hook(path.clone(), true);
    let mut first = fixture.http_push(address, "trunk").await;
    timeout(Duration::from_secs(15), first_reached)
        .await
        .unwrap()
        .unwrap();
    let (second_reached, second_release) = policy_tests::hold_next_hook(path.clone(), true);
    let mut second = fixture.http_push(address, "topic").await;
    timeout(Duration::from_secs(15), second_reached)
        .await
        .unwrap()
        .unwrap();
    first_release.send(()).unwrap();
    let mut reply = Vec::new();
    assert!(
        timeout(Duration::from_millis(300), first.read_to_end(&mut reply))
            .await
            .is_err(),
        "metadata bypassed the repository lock"
    );
    assert_eq!(
        git2::Repository::open_bare(&path)
            .unwrap()
            .find_reference("HEAD")
            .unwrap()
            .symbolic_target(),
        Some("refs/heads/main")
    );

    // Hold the metadata UPDATE, then check that its HEAD selection still owns the lock.
    let mut pg = None;
    let mut sqlite = None;
    match &pool {
        DbPool::Postgres(raw) => {
            let mut transaction = raw.begin().await.unwrap();
            sqlx::query("SELECT id FROM repositories WHERE id=$1 FOR UPDATE")
                .bind(fixture.repo.id)
                .fetch_one(&mut *transaction)
                .await
                .unwrap();
            pg = Some(transaction);
        }
        DbPool::Sqlite(raw) => {
            let mut transaction = raw.begin().await.unwrap();
            sqlx::query("UPDATE repositories SET size_bytes=size_bytes WHERE id=?1")
                .bind(fixture.repo.id)
                .execute(&mut *transaction)
                .await
                .unwrap();
            sqlite = Some(transaction);
        }
    }
    drop(lock);
    timeout(Duration::from_secs(5), async {
        loop {
            if git2::Repository::open_bare(&path).unwrap().head().is_ok() {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(
        timeout(
            Duration::from_millis(200),
            crate::backend::git::locks::lock(&path)
        )
        .await
        .is_err(),
        "HEAD lock was released before its database update"
    );
    second_release.send(()).unwrap();
    if let Some(transaction) = pg {
        transaction.commit().await.unwrap();
    }
    if let Some(transaction) = sqlite {
        transaction.commit().await.unwrap();
    }
    timeout(Duration::from_secs(15), first.read_to_end(&mut reply))
        .await
        .unwrap()
        .unwrap();
    assert!(String::from_utf8_lossy(&reply).contains("ok refs/heads/trunk"));
    reply.clear();
    timeout(Duration::from_secs(15), second.read_to_end(&mut reply))
        .await
        .unwrap()
        .unwrap();
    assert!(String::from_utf8_lossy(&reply).contains("ok refs/heads/topic"));
    stop.send_replace(true);
    timeout(Duration::from_secs(15), running)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    fixture.assert_metadata("trunk").await;
}

async fn sqlite_push_database() -> (TempDir, DbPool) {
    let directory = tempfile::tempdir().unwrap();
    let pool = crate::backend::db::init_db(&format!(
        "sqlite://{}",
        directory.path().join("test.db").display()
    ))
    .await
    .unwrap();
    db::run_migrations(&pool).await.unwrap();
    (directory, pool)
}

#[tokio::test]
async fn disconnected_http_push_drains_postgres() {
    let database = database().await;
    disconnected_push_drains(database.pool.clone(), false).await;
    database.pool.close().await;
}

#[tokio::test]
async fn disconnected_http_push_drains_sqlite() {
    let (_directory, pool) = sqlite_push_database().await;
    disconnected_push_drains(pool.clone(), false).await;
    pool.close().await;
}

#[tokio::test]
async fn disconnected_builtin_ssh_push_drains_postgres() {
    let database = database().await;
    disconnected_push_drains(database.pool.clone(), true).await;
    database.pool.close().await;
}

#[tokio::test]
async fn disconnected_builtin_ssh_push_drains_sqlite() {
    let (_directory, pool) = sqlite_push_database().await;
    disconnected_push_drains(pool.clone(), true).await;
    pool.close().await;
}

#[tokio::test]
async fn concurrent_first_pushes_postgres() {
    let database = database().await;
    concurrent_first_pushes(database.pool.clone()).await;
    database.pool.close().await;
}

#[tokio::test]
async fn concurrent_first_pushes_sqlite() {
    let (_directory, pool) = sqlite_push_database().await;
    concurrent_first_pushes(pool.clone()).await;
    pool.close().await;
}

#[tokio::test]
async fn host_openssh_refuses_remote_repositories_without_starting_git() {
    let database = database().await;
    remote_repository_authorization(&database.pool).await;
}

#[tokio::test]
async fn host_openssh_refuses_remote_repositories_without_starting_git_sqlite() {
    let pool = db::init_db("sqlite::memory:").await.unwrap();
    db::run_migrations(&pool).await.unwrap();
    remote_repository_authorization(&pool).await;
    pool.close().await;
}

async fn remote_repository_authorization(pool: &DbPool) {
    let broker = Broker::new(pool.clone()).await;
    let user = db::users::accounts::create_user(pool, "remote_owner", None)
        .await
        .unwrap();
    let ssh_key = db::users::ssh_keys::add_ssh_key(pool, user.id, "fixture", &key(10))
        .await
        .unwrap();
    let outsider = db::users::accounts::create_user(pool, "outsider", None)
        .await
        .unwrap();
    let outsider_key = db::users::ssh_keys::add_ssh_key(pool, outsider.id, "outsider", &key(11))
        .await
        .unwrap();
    let path = broker
        .gateway
        .repos_root
        .join(format!("{}.git", uuid::Uuid::new_v4()));
    let repo = db::repositories::records::create_repository(
        pool,
        user.id,
        "remote",
        None,
        true,
        path.to_str().unwrap(),
    )
    .await
    .unwrap();
    let node = db::cluster::register_own_node(
        pool,
        &crate::backend::cluster::NodeKey::generate().public_hex(),
        "remote-storage",
        &["storage".to_owned()],
        "test",
    )
    .await
    .unwrap();
    assert!(db::cluster::place_repository(pool, repo.id, node.id)
        .await
        .unwrap());
    for service in ["git-upload-pack", "git-receive-pack"] {
        let hidden = broker
            .serve(
                outsider_key.id,
                &format!("{service} 'remote_owner/remote.git'"),
            )
            .await;
        let missing = broker
            .serve(
                outsider_key.id,
                &format!("{service} 'remote_owner/missing.git'"),
            )
            .await;
        assert_refused(&hidden);
        assert_refused(&missing);
        assert_eq!(hidden.status.code(), missing.status.code());
        assert_eq!(hidden.stdout, missing.stdout);
        assert_eq!(hidden.stderr, missing.stderr);

        let result = broker
            .serve(ssh_key.id, &format!("{service} 'remote_owner/remote.git'"))
            .await;
        assert!(!result.status.success());
        assert!(result.stdout.is_empty());
        let stderr = String::from_utf8_lossy(&result.stderr);
        assert!(
            stderr.contains(
                "Host OpenSSH cannot serve remote repositories; use HTTP or built-in SSH"
            ),
            "{service}: {stderr}"
        );
    }
    assert!(!path.exists());
    broker.no_hooks_left().await;
    broker.stop().await;
}

#[tokio::test]
async fn live_ban_password_revocation_command_and_canonical_path_denials() {
    let database = database().await;
    let broker = Broker::new(database.pool.clone()).await;
    let user = db::users::accounts::create_user(&database.pool, "alice", None)
        .await
        .unwrap();
    let ssh_key = db::users::ssh_keys::add_ssh_key(&database.pool, user.id, "fixture", &key(10))
        .await
        .unwrap();
    let bare_path = broker.gateway.repos_root.join("private.git");
    git2::Repository::init_bare(&bare_path).unwrap();
    let repo = db::repositories::records::create_repository(
        &database.pool,
        user.id,
        "private",
        None,
        true,
        bare_path.to_str().unwrap(),
    )
    .await
    .unwrap();
    for command in [
        "sh",
        "git-upload-pack 'alice/private.git'; id",
        "git-upload-pack alice/../private",
        "git-upload-pack alice/.git",
        "git-upload-pack //alice/private.git",
    ] {
        let result = broker.serve(ssh_key.id, command).await;
        assert!(!result.status.success(), "{command}");
        assert!(
            result.stdout.is_empty(),
            "diagnostics must never enter Git stdout"
        );
    }
    for column in ["is_banned", "must_change_password"] {
        sqlx::query(&format!("UPDATE users SET {column} = TRUE WHERE id = $1"))
            .bind(user.id)
            .execute(&database.native)
            .await
            .unwrap();
        assert!(broker.keys(&key(10), "git").await.stdout.is_empty());
        let result = broker
            .serve(ssh_key.id, "git-upload-pack 'alice/private.git'")
            .await;
        assert!(!result.status.success());
        assert!(result.stdout.is_empty());
        sqlx::query(&format!("UPDATE users SET {column} = FALSE WHERE id = $1"))
            .bind(user.id)
            .execute(&database.native)
            .await
            .unwrap();
    }
    let outside = tempfile::Builder::new()
        .prefix("openssh-outside-")
        .tempdir()
        .unwrap();
    git2::Repository::init_bare(outside.path()).unwrap();
    let escaped = broker.gateway.repos_root.join("escaped.git");
    std::os::unix::fs::symlink(outside.path(), &escaped).unwrap();
    sqlx::query("UPDATE repositories SET disk_path = $1 WHERE id = $2")
        .bind(escaped.to_str().unwrap())
        .bind(repo.id)
        .execute(&database.native)
        .await
        .unwrap();
    let result = broker
        .serve(ssh_key.id, "git-upload-pack 'alice/private.git'")
        .await;
    assert!(!result.status.success());
    assert!(result.stdout.is_empty());
    sqlx::query("UPDATE repositories SET disk_path = $1 WHERE id = $2")
        .bind(bare_path.to_str().unwrap())
        .bind(repo.id)
        .execute(&database.native)
        .await
        .unwrap();
    db::users::ssh_keys::delete_ssh_key(&database.pool, ssh_key.id)
        .await
        .unwrap();
    assert!(broker.keys(&key(10), "git").await.stdout.is_empty());
    let result = broker
        .serve(ssh_key.id, "git-upload-pack 'alice/private.git'")
        .await;
    assert!(!result.status.success());
    assert!(result.stdout.is_empty());
    broker.no_hooks_left().await;
    broker.stop().await;
}

#[tokio::test]
async fn pushes_move_the_open_pull_requests_that_use_the_branch() {
    let database = database().await;
    let broker = Broker::new(database.pool.clone()).await;
    let owner = db::users::accounts::create_user(&database.pool, "alice", None)
        .await
        .unwrap();
    let owner_key = db::users::ssh_keys::add_ssh_key(&database.pool, owner.id, "owner", &key(31))
        .await
        .unwrap();
    let bare_path = broker.gateway.repos_root.join("proj.git");
    let mut options = git2::RepositoryInitOptions::new();
    options.bare(true).initial_head("main");
    let bare = git2::Repository::init_opts(&bare_path, &options).unwrap();
    let repo = db::repositories::records::create_repository(
        &database.pool,
        owner.id,
        "proj",
        None,
        false,
        bare_path.to_str().unwrap(),
    )
    .await
    .unwrap();
    let source_path = broker.directory.path().join("source");
    options.bare(false);
    let source = git2::Repository::init_opts(&source_path, &options).unwrap();
    let push = |refspec: &'static str| {
        let (broker, path, key_id) = (&broker, &source_path, owner_key.id);
        async move {
            let output = broker
                .git(
                    path,
                    key_id,
                    &["push", "git@fixture:alice/proj.git", refspec],
                )
                .await;
            assert!(
                output.status.success(),
                "push {refspec}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    };
    let first = commit(&source, "first\n").to_string();
    push("HEAD:refs/heads/main").await;
    push("HEAD:refs/heads/topic").await;
    let pull = db::pull_requests::records::create_pull_request(
        &database.pool,
        db::pull_requests::records::NewPullRequest {
            head_repo_id: None,
            repo_id: repo.id,
            author_id: owner.id,
            title: "Topic",
            body: "",
            head_branch: "topic",
            head_sha: &first,
            base_branch: "main",
            base_sha: &first,
            is_draft: false,
        },
    )
    .await
    .unwrap();
    let current = || async {
        db::pull_requests::records::get_pull_request_by_id(&database.pool, pull.id)
            .await
            .unwrap()
            .unwrap()
    };

    // A push to the head branch moves the pull request's head commit and revision.
    let second = commit(&source, "second\n").to_string();
    push("HEAD:refs/heads/topic").await;
    let moved = current().await;
    assert_eq!(
        (moved.head_sha.as_str(), moved.base_sha.as_str()),
        (second.as_str(), first.as_str())
    );
    assert!(moved.updated_at > pull.updated_at);

    // A push to the base branch moves its base commit.
    let third = commit(&source, "third\n").to_string();
    push("HEAD:refs/heads/main").await;
    let moved = current().await;
    assert_eq!(
        (moved.head_sha.as_str(), moved.base_sha.as_str()),
        (second.as_str(), third.as_str())
    );

    // Deleting the head branch leaves the pull request open with its last commit.
    // A delete sends no pack, so git hangs up as soon as it has the report. Here the
    // helper is wired straight to git's pipes (no ssh client keeps the channel open),
    // so that can end the session early and the exit status says nothing. The
    // branch and the pull request are what counts.
    broker
        .git(
            &source_path,
            owner_key.id,
            &["push", "git@fixture:alice/proj.git", ":refs/heads/topic"],
        )
        .await;
    broker.no_hooks_left().await;
    assert!(bare.find_reference("refs/heads/topic").is_err());
    let deleted = current().await;
    assert_eq!(deleted.state, "open");
    assert_eq!(deleted.head_sha, second);
    assert_eq!(deleted.updated_at, moved.updated_at);

    // A push that creates the branch again is followed.
    push("HEAD:refs/heads/topic").await;
    assert_eq!(current().await.head_sha, third);
    broker.no_hooks_left().await;
    broker.stop().await;
}
