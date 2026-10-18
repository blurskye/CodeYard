//! A throwaway PostgreSQL schema (from GITSERV_TEST_DATABASE_URL) with one user
//! and one repository, dropped when the fixture ends.
// Each integration crate uses a different subset of these shared test fixtures.
#![allow(dead_code)]
use codeyard::backend::db;
use std::time::Duration;
use uuid::Uuid;

const CLEANUP_PHASE_TIMEOUT: Duration = Duration::from_secs(10);

/// Only fixed labels and a validated five-byte SQLSTATE may cross this boundary.
/// SQLx Display/Debug can contain connection strings, SQL or server payloads.
#[derive(Clone)]
struct CleanupFailure {
    phase: &'static str,
    reason: &'static str,
    class: &'static str,
    sqlstate: Option<String>,
}
impl CleanupFailure {
    fn fixed(phase: &'static str, reason: &'static str) -> Self {
        Self {
            phase,
            reason,
            class: "fixture",
            sqlstate: None,
        }
    }
    fn database(phase: &'static str, error: sqlx::Error) -> Self {
        let class = match &error {
            sqlx::Error::Database(_) => "database",
            sqlx::Error::Configuration(_) => "configuration",
            sqlx::Error::Io(_) => "io",
            sqlx::Error::Tls(_) => "tls",
            sqlx::Error::Protocol(_) => "protocol",
            sqlx::Error::PoolTimedOut => "pool-timeout",
            sqlx::Error::PoolClosed => "pool-closed",
            sqlx::Error::WorkerCrashed => "worker-crashed",
            _ => "sqlx-other",
        };
        let sqlstate = error
            .as_database_error()
            .and_then(|error| error.code())
            .filter(|code| {
                code.len() == 5
                    && code
                        .bytes()
                        .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit())
            })
            .map(|code| code.into_owned());
        Self {
            phase,
            reason: "driver-error",
            class,
            sqlstate,
        }
    }
}
impl std::fmt::Display for CleanupFailure {
    fn fmt(&self, output: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            output,
            "phase={} reason={} class={} sqlstate={} sqlstate-class={}",
            self.phase,
            self.reason,
            self.class,
            self.sqlstate.as_deref().unwrap_or("none"),
            self.sqlstate
                .as_deref()
                .map(|code| &code[..2])
                .unwrap_or("none")
        )
    }
}

async fn cleanup_schema(
    pool: &sqlx::PgPool,
    schema: &str,
    url: &str,
) -> Result<(), CleanupFailure> {
    // Accept exactly the generated test_ci_UUID identifier, never an arbitrary
    // schema or quoted/query-bearing input. Keep the original database URL check.
    let suffix = schema.strip_prefix("test_ci_").unwrap_or("");
    if suffix.len() != 32
        || !suffix
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        || Uuid::parse_str(suffix).is_err()
    {
        return Err(CleanupFailure::fixed("schema-identity", "not-owned"));
    }
    tokio::time::timeout(CLEANUP_PHASE_TIMEOUT, pool.close())
        .await
        .map_err(|_| CleanupFailure::fixed("pool-close", "deadline-10s"))?;
    let options: sqlx::postgres::PgConnectOptions = url
        .parse()
        .map_err(|error| CleanupFailure::database("admin-connect", error))?;
    let admin = tokio::time::timeout(
        CLEANUP_PHASE_TIMEOUT,
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(CLEANUP_PHASE_TIMEOUT)
            .connect_with(options.options([("lock_timeout", "5s"), ("statement_timeout", "10s")])),
    )
    .await
    .map_err(|_| CleanupFailure::fixed("admin-connect", "deadline-10s"))?
    .map_err(|error| CleanupFailure::database("admin-connect", error))?;
    let dropped = tokio::time::timeout(
        CLEANUP_PHASE_TIMEOUT,
        sqlx::query(&format!("DROP SCHEMA \"{schema}\" CASCADE")).execute(&admin),
    )
    .await
    .map_err(|_| CleanupFailure::fixed("drop-schema", "deadline-10s"))
    .and_then(|result| {
        result
            .map(|_| ())
            .map_err(|error| CleanupFailure::database("drop-schema", error))
    });
    let closed = tokio::time::timeout(CLEANUP_PHASE_TIMEOUT, admin.close())
        .await
        .map_err(|_| CleanupFailure::fixed("admin-close", "deadline-10s"));
    // Do not lose either failure if schema removal and admin drain both fail.
    if let (Err(_), Err(error)) = (&dropped, &closed) {
        eprintln!("owned CI schema cleanup also failed: {error}");
    }
    dropped?;
    closed
}

struct Schema {
    pool: sqlx::PgPool,
    name: String,
    url: String,
    removed: bool,
    failure: Option<CleanupFailure>,
}
impl Schema {
    async fn finish(&mut self) -> Result<(), CleanupFailure> {
        if let Some(error) = &self.failure {
            return Err(error.clone());
        }
        if self.removed {
            return Ok(());
        }
        // Cancellation must not silently disarm the destructor or erase failure.
        self.failure = Some(CleanupFailure::fixed(
            "async-cleanup",
            "incomplete-or-cancelled",
        ));
        match cleanup_schema(&self.pool, &self.name, &self.url).await {
            Ok(()) => {
                self.removed = true;
                self.failure = None;
                Ok(())
            }
            Err(error) => {
                self.failure = Some(error.clone());
                Err(error)
            }
        }
    }
}
impl Drop for Schema {
    fn drop(&mut self) {
        if self.removed {
            return;
        }
        let error = if let Some(error) = &self.failure {
            // Never turn an earlier failed/incomplete finish into a green retry.
            error.clone()
        } else {
            let fixture_pool = self.pool.clone();
            let schema = self.name.clone();
            let url = self.url.clone();
            let result = std::thread::Builder::new()
                .name("owned-ci-schema-cleanup".into())
                .spawn(move || {
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .map_err(|_| {
                            CleanupFailure::fixed("cleanup-thread", "runtime-create-failed")
                        })?;
                    runtime.block_on(cleanup_schema(&fixture_pool, &schema, &url))
                });
            match result {
                Ok(thread) => match thread.join() {
                    Ok(Ok(())) => {
                        self.removed = true;
                        return;
                    }
                    Ok(Err(error)) => error,
                    Err(_) => CleanupFailure::fixed("cleanup-thread", "join-panicked"),
                },
                Err(_) => CleanupFailure::fixed("cleanup-thread", "spawn-failed"),
            }
        };
        if std::thread::panicking() {
            eprintln!("owned CI schema cleanup failed: {error}");
        } else {
            panic!("owned CI schema cleanup failed: {error}");
        }
    }
}
pub struct Fixture {
    pub pool: db::DbPool,
    pub raw: sqlx::PgPool,
    pub temp: tempfile::TempDir,
    pub owner: db::models::User,
    pub repo: db::models::Repository,
    pub sha: String,
    _schema: Schema,
}
impl Fixture {
    pub fn scoped_database_url(&self) -> String {
        format!(
            "{}?options=-csearch_path%3D{}",
            self._schema.url, self._schema.name
        )
    }
    pub async fn new() -> Self {
        let url = std::env::var("GITSERV_TEST_DATABASE_URL")
            .expect("set GITSERV_TEST_DATABASE_URL to a PostgreSQL URL");
        let parsed = url::Url::parse(&url).expect("fixture URL");
        assert!(
            matches!(parsed.scheme(), "postgres" | "postgresql")
                && parsed.path() == "/gitserv_test"
                && parsed.query().is_none()
                && parsed.fragment().is_none(),
            "tests only run against a database named gitserv_test"
        );
        let name = format!("test_ci_{}", Uuid::new_v4().simple());
        let admin = sqlx::postgres::PgPoolOptions::new()
            .connect(&url)
            .await
            .expect("required PG fixture unavailable");
        sqlx::query(&format!("CREATE SCHEMA \"{name}\""))
            .execute(&admin)
            .await
            .expect("owned schema");
        // Guard exists before connection/migration: setup failure still cleans
        // exactly this randomly named schema (no production or other tests).
        let mut schema = Schema {
            pool: admin.clone(),
            name,
            url,
            removed: false,
            failure: None,
        };
        admin.close().await;
        schema.pool = sqlx::postgres::PgPoolOptions::new()
            .connect(&format!(
                "{}?options=-csearch_path%3D{}",
                schema.url, schema.name
            ))
            .await
            .expect("scoped PG fixture");
        db::run_migrations(&schema.pool.clone().into())
            .await
            .expect("CI fixture schema migrations");
        let pool: db::DbPool = schema.pool.clone().into();
        let owner = db::users::accounts::create_user(&pool, "ci_owner", None)
            .await
            .expect("fixture owner");
        let temp = tempfile::tempdir().expect("owned tempdir");
        let path = temp.path().join("repo.git");
        let git = git2::Repository::init_bare(&path).expect("owned bare Git");
        let sha = commit(&git, "refs/heads/main", None, "admitted-marker", None);
        let repo = db::repositories::records::create_repository(
            &pool,
            owner.id,
            "fixture",
            None,
            true,
            path.to_str().expect("fixture path"),
        )
        .await
        .expect("fixture repo");
        Self {
            pool,
            raw: schema.pool.clone(),
            temp,
            owner,
            repo,
            sha,
            _schema: schema,
        }
    }
    /// Call on the creating runtime, after all service shutdowns, transaction
    /// commits/rollbacks and task joins. Pool clones may remain, but no borrower
    /// may still own a checked-out connection. Drop is only a failure backstop.
    pub async fn finish(&mut self) {
        if let Err(error) = self._schema.finish().await {
            panic!("owned CI schema cleanup failed: {error}");
        }
    }
    pub async fn count(&self, table: &str) -> i64 {
        assert!(matches!(
            table,
            "ci_runs" | "ci_deployments" | "commit_statuses" | "notifications"
        ));
        sqlx::query_scalar(&format!("SELECT COUNT(*)::BIGINT FROM {table}"))
            .fetch_one(&self.raw)
            .await
            .expect("owned count")
    }
}

pub fn commit(
    git: &git2::Repository,
    reference: &str,
    parent: Option<&str>,
    marker: &str,
    environment: Option<&str>,
) -> String {
    let environment = environment
        .map(|name| format!("    environment: {name}\n"))
        .unwrap_or_default();
    let content = format!("name: {marker}\non: [push, pull_request, workflow_dispatch]\njobs:\n  test:\n    runs-on: ubuntu-latest\n{environment}    steps:\n      - run: echo {marker}\n");
    let blob = git.blob(content.as_bytes()).expect("workflow blob");
    let mut workflows = git.treebuilder(None).expect("workflow tree");
    workflows
        .insert("ci.yml", blob, 0o100644)
        .expect("workflow tree entry");
    let mut github = git.treebuilder(None).expect("github tree");
    github
        .insert(
            "workflows",
            workflows.write().expect("workflow tree write"),
            0o040000,
        )
        .expect("github workflows entry");
    let mut root = git.treebuilder(None).expect("root tree");
    root.insert(
        ".github",
        github.write().expect("github tree write"),
        0o040000,
    )
    .expect("github entry");
    root.insert(
        "marker.txt",
        git.blob(marker.as_bytes()).expect("marker blob"),
        0o100644,
    )
    .expect("marker entry");
    let tree = git
        .find_tree(root.write().expect("root tree write"))
        .expect("commit tree");
    let parents = parent.map(|sha| {
        git.find_commit(git2::Oid::from_str(sha).expect("parent SHA"))
            .expect("parent commit")
    });
    let signature =
        git2::Signature::now("CI fixture", "ci-fixture@example.test").expect("fixture identity");
    let oid = git
        .commit(
            None,
            &signature,
            &signature,
            "CI fixture commit",
            &tree,
            &parents.as_ref().into_iter().collect::<Vec<_>>(),
        )
        .expect("fixture commit");
    git.reference(reference, oid, true, "owned fixture advancement")
        .expect("fixture ref");
    oid.to_string()
}
