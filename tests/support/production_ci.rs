//! A throwaway PostgreSQL schema (from GITSERV_TEST_DATABASE_URL) with one user, one repository
//! and a workflow commit, dropped when the fixture ends.
use crate::support::native_database::PostgresDatabase;
use codeyard::backend::db;

pub struct Fixture {
    pub pool: db::DbPool,
    pub raw: sqlx::PgPool,
    pub temp: tempfile::TempDir,
    pub owner: db::models::User,
    pub repo: db::models::Repository,
    pub sha: String,
    database: PostgresDatabase,
}
impl Fixture {
    pub async fn new() -> Self {
        let database = PostgresDatabase::new().await;
        let pool = database.pool.clone();
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
            raw: database.raw.clone(),
            temp,
            owner,
            repo,
            sha,
            database,
        }
    }
    /// Call on the creating runtime after the servers and tasks that use the database have
    /// stopped. Drop does the same if the test never gets there.
    pub async fn finish(&mut self) {
        self.database.finish().await;
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
