//! Live router and native-form authorization regressions on disposable SQLite,
//! Git and local objects. No private installation or CI executor.
#![cfg(not(target_arch = "wasm32"))]

use axum::{
    body::{to_bytes, Body},
    http::{header, Request, StatusCode},
    Router,
};
use serde_json::{json, Value};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use std::sync::Arc;
use std::{str::FromStr, time::Duration};

// This handler suite needs only a disposable SQLite handle, not the shared
// fixture's unrelated PostgreSQL/reopen API. Keep its ownership local.
struct NativeDatabase {
    pool: db::DbPool,
    raw: sqlx::SqlitePool,
    _directory: tempfile::TempDir,
}
impl NativeDatabase {
    async fn new() -> Self {
        let directory = tempfile::tempdir().expect("owned SQLite handler fixture");
        let url = format!(
            "sqlite://{}",
            directory.path().join("application.sqlite").display()
        );
        let pool = db::init_db(&url).await.expect("native SQLite open");
        db::run_migrations(&pool)
            .await
            .expect("native SQLite migration");
        let raw = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(
                SqliteConnectOptions::from_str(&url)
                    .unwrap()
                    .foreign_keys(true)
                    .journal_mode(SqliteJournalMode::Wal)
                    .synchronous(SqliteSynchronous::Full)
                    .busy_timeout(Duration::from_secs(3)),
            )
            .await
            .expect("owned raw fixture handle");
        Self {
            pool,
            raw,
            _directory: directory,
        }
    }
    async fn close(&self) {
        self.raw.close().await;
        self.pool.close().await;
    }
}
fn token() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}
use codeyard::backend::{
    api::create_router,
    app::AppState,
    db,
    repositories::store::{RepoError, RepoStore},
    storage::ObjectStore,
};
use tower::ServiceExt;

struct Actor {
    user: db::models::User,
    session: String,
    csrf: String,
}
struct Fixture {
    database: NativeDatabase,
    _root: tempfile::TempDir,
    router: Router,
    owner: Actor,
    admin: Actor,
    member: Actor,
    public: db::models::Repository,
    private: db::models::Repository,
    personal: db::models::Repository,
    sha: String,
}
impl Fixture {
    async fn new() -> Self {
        let database = NativeDatabase::new().await;
        let root = tempfile::tempdir().unwrap();
        let owner = Self::actor(&database.pool, "overlapping_user").await;
        let admin = Self::actor(&database.pool, "site_admin").await;
        let member = Self::actor(&database.pool, "member").await;
        sqlx::query("UPDATE users SET is_admin=1 WHERE id=?")
            .bind(admin.user.id)
            .execute(&database.raw)
            .await
            .unwrap();
        // Explicit same-numbered IDs: no organization membership exists for this user.
        sqlx::query("INSERT INTO organizations(id,name) VALUES(?, 'fixture_org')")
            .bind(owner.user.id)
            .execute(&database.raw)
            .await
            .unwrap();
        sqlx::query("INSERT INTO node_configuration(id,setup_completed) VALUES(1,1) ON CONFLICT(id) DO UPDATE SET setup_completed=1")
            .execute(&database.raw).await.unwrap();
        let mut repositories = Vec::new();
        let mut sha = String::new();
        for (name, private, organization) in [
            ("public", false, true),
            ("private", true, true),
            ("personal", true, false),
        ] {
            let path = root.path().join(format!("{name}.git"));
            let git = git2::Repository::init_bare(&path).unwrap();
            let tree_id = git.treebuilder(None).unwrap().write().unwrap();
            let tree = git.find_tree(tree_id).unwrap();
            let signature = git2::Signature::now("Fixture", "fixture@example.test").unwrap();
            let oid = git
                .commit(
                    Some("refs/heads/main"),
                    &signature,
                    &signature,
                    "fixture",
                    &tree,
                    &[],
                )
                .unwrap();
            git.reference("refs/heads/topic", oid, true, "fixture")
                .unwrap();
            git.set_head("refs/heads/main").unwrap();
            sha = oid.to_string();
            let repository = db::repositories::records::create_repository(
                &database.pool,
                owner.user.id,
                name,
                None,
                private,
                path.to_str().unwrap(),
            )
            .await
            .unwrap();
            if organization {
                sqlx::query("UPDATE repositories SET owner_kind='org',owner_user_id=NULL,owner_org_id=? WHERE id=?")
                    .bind(owner.user.id).bind(repository.id).execute(&database.raw).await.unwrap();
            }
            repositories.push(
                db::repositories::records::get_repository_by_id(&database.pool, repository.id)
                    .await
                    .unwrap()
                    .unwrap(),
            );
        }
        let state = AppState::new(
            database.pool.clone(),
            root.path().join("repos"),
            Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
            None,
            codeyard::backend::ci::CiConfig::default(),
            None,
        );
        let router = create_router(state);
        let mut repositories = repositories.into_iter();
        Self {
            database,
            _root: root,
            router,
            owner,
            admin,
            member,
            public: repositories.next().unwrap(),
            private: repositories.next().unwrap(),
            personal: repositories.next().unwrap(),
            sha,
        }
    }
    async fn actor(pool: &db::DbPool, username: &str) -> Actor {
        let user = db::users::accounts::create_user(pool, username, None)
            .await
            .unwrap();
        let session = token();
        let csrf = token();
        db::users::sessions::create_session(
            pool,
            &session,
            user.id,
            None,
            None,
            chrono::Utc::now() + chrono::Duration::hours(1),
        )
        .await
        .unwrap();
        assert_eq!(
            db::users::sessions::get_or_create_csrf_token(
                pool,
                &session,
                Some(user.id),
                &csrf,
                3600
            )
            .await
            .unwrap(),
            Some(csrf.clone())
        );
        Actor {
            user,
            session,
            csrf,
        }
    }
    async fn request(
        &self,
        actor: &Actor,
        method: &str,
        path: &str,
        body: String,
        form: bool,
    ) -> (StatusCode, String) {
        let response = self
            .router
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(path)
                    .header(header::COOKIE, format!("session_id={}", actor.session))
                    .header("x-csrf-token", &actor.csrf)
                    .header(
                        header::ACCEPT,
                        codeyard::webui::bootstrap::MODEL_CONTENT_TYPE,
                    )
                    .header(
                        header::CONTENT_TYPE,
                        if form {
                            "application/x-www-form-urlencoded"
                        } else {
                            "application/json"
                        },
                    )
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let body = to_bytes(response.into_body(), 2 * 1024 * 1024)
            .await
            .unwrap();
        (status, String::from_utf8(body.to_vec()).unwrap())
    }
    async fn api(
        &self,
        actor: &Actor,
        method: &str,
        path: &str,
        body: Value,
    ) -> (StatusCode, Value) {
        let (status, body) = self
            .request(actor, method, path, body.to_string(), false)
            .await;
        (
            status,
            if body.is_empty() {
                Value::Null
            } else {
                serde_json::from_str(&body).expect("real API JSON")
            },
        )
    }
    async fn form(&self, actor: &Actor, path: &str, fields: &[(&str, &str)]) -> StatusCode {
        let encoded = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("csrf_token", &actor.csrf)
            .extend_pairs(fields.iter().copied())
            .finish();
        self.request(actor, "POST", path, encoded, true).await.0
    }
    async fn page(&self, actor: &Actor, path: &str) -> (StatusCode, Value) {
        let (status, body) = self.request(actor, "GET", path, String::new(), false).await;
        (
            status,
            serde_json::from_str(&body).expect("authorized safe page model"),
        )
    }
    async fn enable_governance(&self) {
        let governor =
            db::users::accounts::create_user(&self.database.pool, "policy_governor", None)
                .await
                .unwrap();
        sqlx::query("INSERT INTO org_memberships(org_id,user_id,role) VALUES(?1,?2,'owner')")
            .bind(self.owner.user.id)
            .bind(governor.id)
            .execute(&self.database.raw)
            .await
            .unwrap();
        let unrelated_memberships: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM org_memberships WHERE org_id=?1 AND user_id=?2",
        )
        .bind(self.owner.user.id)
        .bind(self.owner.user.id)
        .fetch_one(&self.database.raw)
        .await
        .unwrap();
        assert_eq!(unrelated_memberships, 0);
    }
    async fn review(&self, repository: &db::models::Repository) -> (i32, i64) {
        // Historical discussion data in this fixture, not an authorized
        // contribution from the same-numbered unrelated account.
        let pull = sqlx::query_as::<_, db::models::PullRequest>(
            "INSERT INTO pull_requests(repo_id,number,author_id,title,body,head_branch,head_sha,base_branch,base_sha,is_draft) VALUES(?1,(SELECT COALESCE(MAX(number),0)+1 FROM pull_requests WHERE repo_id=?1),?2,'Fixture pull','','topic',?3,'main',?3,TRUE) RETURNING *",
        ).bind(repository.id).bind(self.member.user.id).bind(&self.sha)
            .fetch_one(&self.database.raw).await.unwrap();
        let review = sqlx::query_as::<_, db::models::PullRequestReview>(
            "INSERT INTO pr_reviews(pull_request_id,author_id,state,commit_sha) VALUES(?1,?2,'approved',?3) RETURNING *",
        ).bind(pull.id).bind(self.admin.user.id).bind(&self.sha)
            .fetch_one(&self.database.raw).await.unwrap();
        (pull.number, review.id)
    }
}

async fn denied_owner_operations(fixture: &Fixture, repository: &db::models::Repository) {
    assert_eq!(repository.owner_kind, "org");
    assert_eq!(repository.owner_org_id, Some(fixture.owner.user.id));
    db::repositories::access::grant_repo_access(
        &fixture.database.pool,
        fixture.member.user.id,
        repository.id,
        db::models::AccessLevel::Read,
    )
    .await
    .unwrap();
    let (number, review) = fixture.review(repository).await;
    // Insert a previous protection rule; denied updates must preserve it.
    sqlx::query("INSERT INTO branch_protection_rules(repo_id,pattern,require_pull_request,required_approving_reviews) VALUES(?1,'main',TRUE,1) ON CONFLICT(repo_id,pattern) DO UPDATE SET require_pull_request=TRUE,required_approving_reviews=1")
        .bind(repository.id).execute(&fixture.database.raw).await.unwrap();
    let base = format!("/api/repos/fixture_org/{}", repository.name);
    for (method, path, body) in [
        ("GET", format!("{base}/collaborators"), json!({})),
        (
            "POST",
            format!("{base}/collaborators"),
            json!({"username":"member","access_level":"admin"}),
        ),
        ("DELETE", format!("{base}/collaborators/member"), json!({})),
        (
            "PUT",
            format!("{base}/branch-protection"),
            json!({"pattern":"outsider","require_pull_request":true}),
        ),
        (
            "DELETE",
            format!("{base}/branch-protection/main"),
            json!({}),
        ),
        (
            "POST",
            format!("{base}/pulls/{number}/reviews/{review}/dismiss"),
            json!({"reason":"outsider"}),
        ),
    ] {
        let (status, body) = fixture.api(&fixture.owner, method, &path, body).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{method} {path}: {body}");
    }
    assert_eq!(
        db::repositories::access::get_user_repo_access(
            &fixture.database.pool,
            fixture.member.user.id,
            repository.id
        )
        .await
        .unwrap(),
        db::models::AccessLevel::Read
    );
    let rules = db::repositories::protection::list_branch_protection_rules(
        &fixture.database.pool,
        repository.id,
    )
    .await
    .unwrap();
    assert_eq!(rules.len(), 1);
    assert_eq!(rules[0].pattern, "main");
    assert_eq!(rules[0].required_approving_reviews, 1);
    assert!(db::pull_requests::reviews::get_pull_request_review_by_id(
        &fixture.database.pool,
        review
    )
    .await
    .unwrap()
    .unwrap()
    .dismissed_by_id
    .is_none());
}

#[tokio::test]
async fn public_org_equal_user_id_does_not_authorize_live_owner_handlers() {
    let fixture = Fixture::new().await;
    fixture.enable_governance().await;
    denied_owner_operations(&fixture, &fixture.public).await;
    let (status, permissions) = fixture
        .api(
            &fixture.owner,
            "GET",
            "/api/repos/fixture_org/public/permissions",
            json!({}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(permissions["role"], "read");
    assert_eq!(permissions["can_write"], false);
    assert_eq!(permissions["can_admin"], false);
    fixture.database.close().await;
}

#[tokio::test]
async fn private_org_equal_user_id_denied_even_with_explicit_read_access() {
    let fixture = Fixture::new().await;
    fixture.enable_governance().await;
    // Private visibility already fails closed, but a read grant must not resurrect the collision.
    denied_owner_operations(&fixture, &fixture.private).await;
    db::repositories::access::grant_repo_access(
        &fixture.database.pool,
        fixture.owner.user.id,
        fixture.private.id,
        db::models::AccessLevel::Read,
    )
    .await
    .unwrap();
    denied_owner_operations(&fixture, &fixture.private).await;
    fixture.database.close().await;
}

#[tokio::test]
async fn native_forms_and_page_controls_do_not_treat_org_id_as_personal_owner() {
    let fixture = Fixture::new().await;
    fixture.enable_governance().await;
    assert_eq!(
        fixture
            .page(&fixture.owner, "/fixture_org/private/settings")
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    db::repositories::access::grant_repo_access(
        &fixture.database.pool,
        fixture.owner.user.id,
        fixture.private.id,
        db::models::AccessLevel::Read,
    )
    .await
    .unwrap();
    for repository in [&fixture.public, &fixture.private] {
        let base = format!("/fixture_org/{}", repository.name);
        let repository_id = repository.id.to_string();
        assert_eq!(
            fixture
                .page(&fixture.owner, &format!("{base}/settings"))
                .await
                .0,
            StatusCode::FORBIDDEN
        );
        for (action, fields) in [
            (
                "collaborator",
                vec![
                    ("repository_id", repository_id.as_str()),
                    ("username", "member"),
                    ("access", "admin"),
                ],
            ),
            (
                "remove-collaborator",
                vec![
                    ("repository_id", repository_id.as_str()),
                    ("username", "member"),
                    ("confirm", "yes"),
                ],
            ),
            (
                "settings",
                vec![
                    ("repository_id", repository_id.as_str()),
                    ("visibility", "public"),
                    ("confirm", "yes"),
                ],
            ),
        ] {
            assert_eq!(
                fixture
                    .form(&fixture.owner, &format!("{base}/_forms/{action}"), &fields)
                    .await,
                StatusCode::FORBIDDEN,
                "{base}/{action}"
            );
        }
        let (status, actions) = fixture
            .page(&fixture.owner, &format!("{base}/actions"))
            .await;
        assert_eq!(
            status,
            StatusCode::OK,
            "{base}/actions: public error category {}",
            actions["page"]["model"]["message"]
        );
        assert_eq!(actions["page"]["model"]["Actions"]["can_dispatch"], false);
        let (status, collaboration) = fixture
            .page(&fixture.owner, &format!("{base}/issues"))
            .await;
        assert_eq!(status, StatusCode::OK);
        let repository = &collaboration["page"]["model"]["List"]["repository"];
        assert_eq!(repository["can_write"], false);
        assert_eq!(repository["can_admin"], false);
        assert_eq!(repository["chrome"]["is_owner"], false);
    }
    // The same user is still the legitimate personal owner on its own repository.
    let base = "/overlapping_user/personal";
    let (status, page) = fixture
        .page(&fixture.owner, &format!("{base}/settings"))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page["page"]["model"]["Settings"]["repo"]["is_owner"], true);
    assert_eq!(
        fixture
            .form(
                &fixture.owner,
                &format!("{base}/_forms/collaborator"),
                &[
                    ("repository_id", &fixture.personal.id.to_string()),
                    ("username", "member"),
                    ("access", "write")
                ]
            )
            .await,
        StatusCode::SEE_OTHER
    );
    assert_eq!(
        db::repositories::access::get_user_repo_access(
            &fixture.database.pool,
            fixture.member.user.id,
            fixture.personal.id
        )
        .await
        .unwrap(),
        db::models::AccessLevel::Write
    );
    assert_eq!(
        fixture
            .form(
                &fixture.owner,
                &format!("{base}/_forms/remove-collaborator"),
                &[
                    ("repository_id", &fixture.personal.id.to_string()),
                    ("username", "member"),
                    ("confirm", "yes")
                ]
            )
            .await,
        StatusCode::SEE_OTHER
    );
    let (status, page) = fixture
        .page(&fixture.admin, &format!("{base}/settings"))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page["page"]["model"]["Settings"]["repo"]["can_admin"], true);
    fixture.database.close().await;
}

#[tokio::test]
async fn personal_owner_and_site_admin_keep_personal_authority_without_tenant_override() {
    let fixture = Fixture::new().await;
    for (actor, owner, repository) in [
        (&fixture.owner, "overlapping_user", &fixture.personal),
        (&fixture.admin, "overlapping_user", &fixture.personal),
        (&fixture.admin, "fixture_org", &fixture.public),
        (&fixture.admin, "fixture_org", &fixture.private),
    ] {
        let base = format!("/api/repos/{owner}/{}", repository.name);
        // The organization fixture deliberately has no usable owner or membership.
        // Platform administration is not organization repository authority.
        let expected = if repository.owner_kind == "org" {
            StatusCode::NOT_FOUND
        } else {
            StatusCode::OK
        };
        assert_eq!(
            fixture
                .api(
                    actor,
                    "PUT",
                    &format!("{base}/branch-protection"),
                    json!({"pattern":"main"})
                )
                .await
                .0,
            expected
        );
        let (number, review) = fixture.review(repository).await;
        let (status, body) = fixture
            .api(
                actor,
                "POST",
                &format!("{base}/pulls/{number}/reviews/{review}/dismiss"),
                json!({"reason":"legitimate"}),
            )
            .await;
        assert_eq!(status, expected, "{body}");
        if expected == StatusCode::OK {
            assert_eq!(body["dismissed_by_id"], actor.user.id);
        } else {
            let dismissed: Option<i64> =
                sqlx::query_scalar("SELECT dismissed_by_id FROM pr_reviews WHERE id=?")
                    .bind(review)
                    .fetch_one(&fixture.database.raw)
                    .await
                    .unwrap();
            assert!(
                dismissed.is_none(),
                "denied organization review must remain unchanged"
            );
        }
        assert_eq!(
            fixture
                .api(
                    actor,
                    "DELETE",
                    &format!("{base}/branch-protection/main"),
                    json!({})
                )
                .await
                .0,
            if expected == StatusCode::OK {
                StatusCode::NO_CONTENT
            } else {
                expected
            }
        );
    }
    // Collaborator policy is deliberately still personal-owner-only, even for a site admin.
    assert_eq!(
        fixture
            .api(
                &fixture.admin,
                "GET",
                "/api/repos/overlapping_user/personal/collaborators",
                json!({})
            )
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    fixture.database.close().await;
}

#[tokio::test]
async fn credentialed_private_blob_and_denials_cannot_enter_a_shared_cache() {
    let fixture = Fixture::new().await;
    let git = git2::Repository::open_bare(&fixture.personal.disk_path).unwrap();
    let blob = git.blob(b"private fixture\n").unwrap();
    let mut tree = git.treebuilder(None).unwrap();
    tree.insert("README.md", blob, 0o100644).unwrap();
    let tree = git.find_tree(tree.write().unwrap()).unwrap();
    let parent = git.find_commit(fixture.sha.parse().unwrap()).unwrap();
    let signature = git2::Signature::now("Fixture", "fixture@example.test").unwrap();
    git.commit(
        Some("refs/heads/main"),
        &signature,
        &signature,
        "private README",
        &tree,
        &[&parent],
    )
    .unwrap();
    let private = "/api/repos/overlapping_user/personal/blob/main/README.md";
    for (path, cookie, authorization, expected, sensitive) in [
        (
            private,
            Some(fixture.owner.session.as_str()),
            None,
            StatusCode::OK,
            true,
        ),
        (
            private,
            Some(fixture.member.session.as_str()),
            None,
            StatusCode::NOT_FOUND,
            true,
        ),
        (private, None, None, StatusCode::NOT_FOUND, false),
        (
            "/api/repos/fixture_org/public",
            None,
            None,
            StatusCode::OK,
            false,
        ),
        (
            "/api/repos/fixture_org/public",
            None,
            Some("Bearer fixture"),
            StatusCode::OK,
            true,
        ),
    ] {
        let mut request = Request::builder().uri(path);
        if let Some(cookie) = cookie {
            request = request.header(header::COOKIE, format!("session_id={cookie}"));
        }
        if let Some(auth) = authorization {
            request = request.header(header::AUTHORIZATION, auth);
        }
        let response = fixture
            .router
            .clone()
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), expected, "{path}");
        if sensitive {
            assert_eq!(
                response.headers()[header::CACHE_CONTROL],
                "no-store, private"
            );
            assert_eq!(response.headers()[header::VARY], "Cookie, Authorization");
            assert_eq!(
                response.headers()[header::X_CONTENT_TYPE_OPTIONS],
                "nosniff"
            );
        } else {
            assert!(
                response.headers().get(header::CACHE_CONTROL).is_none(),
                "public anonymous read must keep its existing cache policy"
            );
        }
        let body = to_bytes(response.into_body(), 2 * 1024 * 1024)
            .await
            .unwrap();
        if path == private && expected == StatusCode::OK {
            assert!(String::from_utf8_lossy(&body).contains("private fixture"));
        } else if path == private {
            assert!(!String::from_utf8_lossy(&body).contains("private fixture"));
        }
    }
    fixture.database.close().await;
}

#[tokio::test]
async fn collaborator_api_projects_canonical_levels_without_changing_privileges_or_inputs() {
    let fixture = Fixture::new().await;
    let base = "/api/repos/overlapping_user/personal";
    let mut actors = Vec::new();
    for (level, name) in [
        (db::models::AccessLevel::None, "none"),
        (db::models::AccessLevel::Read, "read"),
        (db::models::AccessLevel::Triage, "triage"),
        (db::models::AccessLevel::Write, "write"),
        (db::models::AccessLevel::Maintain, "maintain"),
        (db::models::AccessLevel::Admin, "admin"),
        (db::models::AccessLevel::Owner, "owner"),
    ] {
        let actor = Fixture::actor(&fixture.database.pool, &format!("level_{name}")).await;
        db::repositories::access::grant_repo_access(
            &fixture.database.pool,
            actor.user.id,
            fixture.personal.id,
            level,
        )
        .await
        .unwrap();
        actors.push((actor, level, name));
    }
    let (status, collaborators) = fixture
        .api(
            &fixture.owner,
            "GET",
            &format!("{base}/collaborators"),
            json!({}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    // Public visibility lets None actors read permissions without granting write/admin.
    sqlx::query("UPDATE repositories SET is_private=0 WHERE id=?")
        .bind(fixture.personal.id)
        .execute(&fixture.database.raw)
        .await
        .unwrap();
    for (actor, level, name) in &actors {
        let row = collaborators
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["user_id"] == actor.user.id)
            .expect("persisted collaborator");
        assert_eq!(row["access_level"], *name);
        // Persisted collaborator projections stay faithful, but public Read is
        // a floor and a legacy direct Owner value is not ownership authority.
        let effective = if matches!(
            level,
            db::models::AccessLevel::None | db::models::AccessLevel::Owner
        ) {
            db::models::AccessLevel::Read
        } else {
            *level
        };
        let effective_name = if effective == db::models::AccessLevel::Read {
            "read"
        } else {
            *name
        };
        assert_eq!(
            db::repositories::access::get_user_repo_access(
                &fixture.database.pool,
                actor.user.id,
                fixture.personal.id
            )
            .await
            .unwrap(),
            effective
        );
        let (status, permissions) = fixture
            .api(actor, "GET", &format!("{base}/permissions"), json!({}))
            .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(permissions["role"], effective_name);
        assert_eq!(permissions["can_write"], effective.can_write());
        assert_eq!(permissions["can_admin"], effective.can_admin());
    }
    // Existing supported add inputs and the legacy default/fallback keep their semantics.
    for (input, expected) in [
        (Some("read"), "read"),
        (Some("write"), "write"),
        (Some("admin"), "admin"),
        (None, "read"),
        (Some("unknown"), "read"),
    ] {
        let (status, response) = fixture
            .api(
                &fixture.owner,
                "POST",
                &format!("{base}/collaborators"),
                json!({"username":"member", "access_level":input}),
            )
            .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(response["access_level"], expected);
        let (_, list) = fixture
            .api(
                &fixture.owner,
                "GET",
                &format!("{base}/collaborators"),
                json!({}),
            )
            .await;
        assert_eq!(
            list.as_array()
                .unwrap()
                .iter()
                .find(|row| row["username"] == "member")
                .unwrap()["access_level"],
            expected
        );
    }
    assert_eq!(
        fixture
            .api(
                &fixture.owner,
                "POST",
                &format!("{base}/collaborators"),
                json!({"username":"overlapping_user"})
            )
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        fixture
            .api(
                &fixture.owner,
                "DELETE",
                &format!("{base}/collaborators/member"),
                json!({})
            )
            .await
            .0,
        StatusCode::NO_CONTENT
    );
    fixture.database.close().await;
}

#[tokio::test]
async fn organization_engagement_forks_and_pulls_never_notify_same_numbered_user() {
    let fixture = Fixture::new().await;
    for action in ["star", "watch"] {
        assert_eq!(
            fixture
                .api(
                    &fixture.member,
                    "PUT",
                    &format!("/api/repos/fixture_org/public/{action}"),
                    json!({})
                )
                .await
                .0,
            StatusCode::OK
        );
    }
    assert_eq!(
        fixture
            .api(
                &fixture.member,
                "POST",
                "/api/repos/fixture_org/public/forks",
                json!({})
            )
            .await
            .0,
        StatusCode::CREATED
    );
    let request = json!({"title":"Fixture draft", "head_branch":"topic", "base_branch":"main", "is_draft":true});
    // A public read floor is not contribution authority under suspended governance.
    assert_eq!(
        fixture
            .api(
                &fixture.member,
                "POST",
                "/api/repos/fixture_org/public/pulls",
                request.clone()
            )
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    // Give only this owned fixture usable governance, distinct from both the
    // equal-numbered unrelated user and the contributing public Read actor.
    let governor =
        db::users::accounts::create_user(&fixture.database.pool, "engagement_governor", None)
            .await
            .unwrap();
    sqlx::query("INSERT INTO org_memberships(org_id,user_id,role) VALUES(?1,?2,'owner')")
        .bind(fixture.owner.user.id)
        .bind(governor.id)
        .execute(&fixture.database.raw)
        .await
        .unwrap();
    assert_eq!(
        fixture
            .api(
                &fixture.member,
                "POST",
                "/api/repos/fixture_org/public/pulls",
                request
            )
            .await
            .0,
        StatusCode::CREATED
    );
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM notifications WHERE user_id=?")
        .bind(fixture.owner.user.id)
        .fetch_one(&fixture.database.raw)
        .await
        .unwrap();
    assert_eq!(
        count, 0,
        "organization events must not use the unrelated user's ID"
    );
    // An equal-numbered user can fork a public org repository; it is not their own.
    assert_eq!(
        fixture
            .api(
                &fixture.owner,
                "POST",
                "/api/repos/fixture_org/public/forks",
                json!({})
            )
            .await
            .0,
        StatusCode::CREATED
    );
    // Personal repository notifications still reach the legitimate owner.
    db::repositories::access::grant_repo_access(
        &fixture.database.pool,
        fixture.member.user.id,
        fixture.personal.id,
        db::models::AccessLevel::Read,
    )
    .await
    .unwrap();
    for action in ["star", "watch"] {
        assert_eq!(
            fixture
                .api(
                    &fixture.member,
                    "PUT",
                    &format!("/api/repos/overlapping_user/personal/{action}"),
                    json!({})
                )
                .await
                .0,
            StatusCode::OK
        );
    }
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM notifications WHERE user_id=?")
        .bind(fixture.owner.user.id)
        .fetch_one(&fixture.database.raw)
        .await
        .unwrap();
    assert_eq!(count, 2);
    fixture.database.close().await;
}

#[tokio::test]
async fn scoped_repository_projections_never_attribute_org_identity_to_unrelated_user() {
    let fixture = Fixture::new().await;
    let (status, repositories) = fixture
        .api(&fixture.owner, "GET", "/api/repos", json!({}))
        .await;
    assert_eq!(status, StatusCode::OK);
    let org = repositories
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == fixture.public.id)
        .unwrap();
    assert_eq!(
        org["owner"], "fixture_org",
        "canonical organization namespace, never the same-numbered user"
    );
    sqlx::query("UPDATE repositories SET is_fork=1,forked_from_id=? WHERE id=?")
        .bind(fixture.public.id)
        .bind(fixture.personal.id)
        .execute(&fixture.database.raw)
        .await
        .unwrap();
    let (status, repository) = fixture
        .api(
            &fixture.owner,
            "GET",
            "/api/repos/overlapping_user/personal",
            json!({}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(repository["forked_from"]["owner"], "fixture_org");
    assert_eq!(repository["forked_from"]["full_name"], "fixture_org/public");
    let (status, page) = fixture
        .page(&fixture.owner, "/overlapping_user/personal/settings")
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        page["page"]["model"]["Settings"]["repo"]["parent_name"],
        "fixture_org/public"
    );
    assert_eq!(
        page["page"]["model"]["Settings"]["repo"]["parent_url"],
        "/fixture_org/public"
    );
    fixture.database.close().await;
}

#[tokio::test]
async fn empty_repository_actions_are_available_without_dispatch_authority() {
    let fixture = Fixture::new().await;
    {
        let repository = git2::Repository::open_bare(&fixture.public.disk_path).unwrap();
        repository
            .find_reference("refs/heads/main")
            .unwrap()
            .delete()
            .unwrap();
        repository
            .find_reference("refs/heads/topic")
            .unwrap()
            .delete()
            .unwrap();
    }
    let (status, page) = fixture
        .page(&fixture.owner, "/fixture_org/public/actions")
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page["page"]["model"]["Actions"]["can_dispatch"], false);
    assert!(page["page"]["model"]["Actions"]["workflows"]
        .as_array()
        .unwrap()
        .is_empty());
    fixture.database.close().await;
}

#[tokio::test]
async fn valid_ref_without_workflow_directory_has_empty_actions_not_outage() {
    let directory = tempfile::tempdir().unwrap();
    let repository = git2::Repository::init_bare(directory.path()).unwrap();
    let tree_id = repository.treebuilder(None).unwrap().write().unwrap();
    let tree = repository.find_tree(tree_id).unwrap();
    let signature = git2::Signature::now("Fixture", "fixture@example.test").unwrap();
    repository
        .commit(
            Some("refs/heads/main"),
            &signature,
            &signature,
            "no workflows",
            &tree,
            &[],
        )
        .unwrap();
    let store = RepoStore::from_disk_path(directory.path().to_str().unwrap());
    let workflows = codeyard::backend::ci::list_workflows_for_ref(&store, "main").await;
    let kind = workflows
        .as_ref()
        .err()
        .and_then(|error| error.downcast_ref::<RepoError>())
        .map(|error| error.kind);
    assert!(
        workflows.is_ok(),
        "existing commit with absent workflow directory: {kind:?}"
    );
    assert!(workflows.unwrap().is_empty());
    // A nonexistent ref or missing repository must remain an error, not be
    // mistaken for an existing commit without workflow definitions.
    assert!(
        codeyard::backend::ci::list_workflows_for_ref(&store, "missing-ref")
            .await
            .is_err()
    );
    let missing = directory.path().join("missing-repository");
    let missing = RepoStore::from_disk_path(missing.to_str().unwrap());
    assert!(
        codeyard::backend::ci::list_workflows_for_ref(&missing, "main")
            .await
            .is_err()
    );
}

#[tokio::test]
async fn deleting_a_repository_does_not_claim_success_when_its_files_remain() {
    let fixture = Fixture::new().await;
    let owner = &fixture.owner;
    let pool = &fixture.database.pool;
    let base = fixture._root.path();

    // A directory that can be removed: the delete succeeds and the files are gone.
    let removable = base.join("removable.git");
    git2::Repository::init_bare(&removable).unwrap();
    db::repositories::records::create_repository(
        pool,
        owner.user.id,
        "removable",
        None,
        true,
        removable.to_str().unwrap(),
    )
    .await
    .unwrap();
    let (status, _) = fixture
        .api(
            owner,
            "DELETE",
            "/api/repos/overlapping_user/removable",
            json!({}),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(!removable.exists());
    assert!(
        db::repositories::records::get_repository(pool, "overlapping_user", "removable")
            .await
            .unwrap()
            .is_none()
    );

    // Storage that cannot be removed (its parent is a file, not a directory): the
    // repository row is already gone and cannot come back, so the answer is an
    // error that says the files remain, never a success.
    std::fs::write(base.join("blocker"), b"not a directory").unwrap();
    let stuck = base.join("blocker").join("stuck.git");
    db::repositories::records::create_repository(
        pool,
        owner.user.id,
        "stuck",
        None,
        true,
        stuck.to_str().unwrap(),
    )
    .await
    .unwrap();
    let (status, body) = fixture
        .api(
            owner,
            "DELETE",
            "/api/repos/overlapping_user/stuck",
            json!({}),
        )
        .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["error"], "storage_unavailable");
    let message = body["message"].as_str().unwrap();
    assert!(message.contains("deleted") && message.contains("could not be removed"));
    assert!(
        !message.contains(base.to_str().unwrap()),
        "the answer must not disclose where the files are"
    );
    assert!(
        db::repositories::records::get_repository(pool, "overlapping_user", "stuck")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn the_unscoped_ci_event_socket_stays_gone() {
    // The old socket sent every repository's events to every connection. It stays
    // registered only to answer clearly; runs and logs come from the stored rows.
    let fixture = Fixture::new().await;
    let (status, body) = fixture
        .request(&fixture.owner, "GET", "/api/ci/ws", String::new(), false)
        .await;
    assert_eq!(status, StatusCode::GONE);
    assert!(body.contains("has been disabled"));
}
