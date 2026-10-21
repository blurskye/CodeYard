//! Owned fixture rows only; no production organization creation or storage conversion.
use super::{database::Database, native};
use axum::{
    body::{to_bytes, Body},
    http::{header, Request, StatusCode},
    Router,
};
use codeyard::{
    backend::{api::create_router, app::AppState, db, storage::ObjectStore},
    webui::app::{Page, PageEnvelope},
};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Once,
};
use tower::ServiceExt;
use tracing::Instrument;
use tracing_subscriber::{layer::Context, prelude::*, registry::LookupSpan, Layer, Registry};

// Counts SQL statements issued while one request runs, so tests can pin query bounds.
struct QueryCounter;

impl Layer<Registry> for QueryCounter {
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        id: &tracing::Id,
        ctx: Context<'_, Registry>,
    ) {
        if attrs.metadata().name() == "discovery_request" {
            ctx.span(id)
                .unwrap()
                .extensions_mut()
                .insert(AtomicUsize::new(0));
        }
    }

    fn on_event(&self, event: &tracing::Event<'_>, ctx: Context<'_, Registry>) {
        if event.metadata().target() == "sqlx::query" {
            if let Some(scope) = ctx.event_scope(event) {
                for span in scope {
                    if let Some(count) = span.extensions().get::<AtomicUsize>() {
                        count.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }
    }
}

pub async fn count_queries<T>(request: impl std::future::Future<Output = T>) -> (T, usize) {
    static INIT: Once = Once::new();
    INIT.call_once(|| tracing_subscriber::registry().with(QueryCounter).init());
    let span = tracing::info_span!("discovery_request");
    let response = request.instrument(span.clone()).await;
    let count = span
        .with_subscriber(|(id, dispatch)| {
            let registry = dispatch.downcast_ref::<Registry>().unwrap();
            let span = registry.span(id).unwrap();
            let extensions = span.extensions();
            extensions
                .get::<AtomicUsize>()
                .unwrap()
                .load(Ordering::Relaxed)
        })
        .unwrap();
    assert!(count > 0, "SQL query tracing must be active");
    (response, count)
}

pub const COLLISION: i64 = 700_000;
pub const USER: &str = "discovery_user";
pub const ORG: &str = "discovery_org";
pub const PRIVATE: &str = "namespace-secret@example.test";

impl Database {
    pub async fn insert_repository_row(&self, kind: &str, name: &str, private: bool) -> i64 {
        // Direct inserts set the owner as given, not personal-to-org conversion.
        let sql = "INSERT INTO repositories(owner_kind,owner_user_id,owner_org_id,name,is_private,disk_path) VALUES($1,$2,$3,$4,$5,'/fixture/not-executed.git') RETURNING id";
        let user = (kind == "user").then_some(COLLISION);
        let org = (kind == "org").then_some(COLLISION);
        match self {
            Self::Sqlite(f) => sqlx::query_scalar(sql)
                .bind(kind)
                .bind(user)
                .bind(org)
                .bind(name)
                .bind(private)
                .fetch_one(&f.raw)
                .await
                .unwrap(),
            Self::Postgres(f) => sqlx::query_scalar(sql)
                .bind(kind)
                .bind(user)
                .bind(org)
                .bind(name)
                .bind(private)
                .fetch_one(&f.raw)
                .await
                .unwrap(),
        }
    }
    pub async fn set_disk_path(&self, repo: i64, path: &std::path::Path) {
        let sql = "UPDATE repositories SET disk_path=$1 WHERE id=$2";
        match self {
            Self::Sqlite(f) => {
                sqlx::query(sql)
                    .bind(path.to_str().unwrap())
                    .bind(repo)
                    .execute(&f.raw)
                    .await
                    .unwrap();
            }
            Self::Postgres(f) => {
                sqlx::query(sql)
                    .bind(path.to_str().unwrap())
                    .bind(repo)
                    .execute(&f.raw)
                    .await
                    .unwrap();
            }
        }
    }
    pub async fn owner_shape(
        &self,
        repo: i64,
        kind: &str,
        user: Option<i64>,
        org: Option<i64>,
    ) -> Result<(), sqlx::Error> {
        let sql =
            "UPDATE repositories SET owner_kind=$1,owner_user_id=$2,owner_org_id=$3 WHERE id=$4";
        match self {
            Self::Sqlite(f) => {
                sqlx::query(sql)
                    .bind(kind)
                    .bind(user)
                    .bind(org)
                    .bind(repo)
                    .execute(&f.raw)
                    .await?;
            }
            Self::Postgres(f) => {
                sqlx::query(sql)
                    .bind(kind)
                    .bind(user)
                    .bind(org)
                    .bind(repo)
                    .execute(&f.raw)
                    .await?;
            }
        }
        Ok(())
    }
    pub async fn corrupt_shape(&self, repo: i64, kind: &str, user: Option<i64>, org: Option<i64>) {
        // Only an owned disposable fixture admits the malformed shape.
        // FKs/uniqueness remain and only ownership fields change; no production repair.
        match self {
            Self::Postgres(_) => {
                self.execute(
                    "ALTER TABLE repositories DROP CONSTRAINT repo_owner_check",
                    &[],
                )
                .await;
                self.owner_shape(repo, kind, user, org).await.unwrap();
            }
            Self::Sqlite(f) => {
                let mut connection = f.raw.acquire().await.unwrap();
                sqlx::query("PRAGMA ignore_check_constraints=ON")
                    .execute(&mut *connection)
                    .await
                    .unwrap();
                let result = sqlx::query("UPDATE repositories SET owner_kind=$1,owner_user_id=$2,owner_org_id=$3 WHERE id=$4")
                    .bind(kind)
                    .bind(user)
                    .bind(org)
                    .bind(repo)
                    .execute(&mut *connection)
                    .await;
                sqlx::query("PRAGMA ignore_check_constraints=OFF")
                    .execute(&mut *connection)
                    .await
                    .unwrap();
                result.unwrap();
            }
        }
    }
    pub async fn restore_owner_check(&self, repo: i64) {
        self.owner_shape(repo, "user", Some(COLLISION), None)
            .await
            .unwrap();
        if matches!(self, Self::Postgres(_)) {
            self.execute("ALTER TABLE repositories ADD CONSTRAINT repo_owner_check CHECK ((owner_kind='user' AND owner_user_id IS NOT NULL AND owner_org_id IS NULL) OR (owner_kind='org' AND owner_org_id IS NOT NULL AND owner_user_id IS NULL))", &[]).await;
        }
    }
}

pub struct Reply {
    pub status: StatusCode,
    pub envelope: PageEnvelope,
    pub body: String,
}
impl Reply {
    pub fn discovery(self) -> codeyard::webui::pages::discovery::Page {
        assert_eq!(self.status, StatusCode::OK, "{}", self.body);
        assert!(!self.body.contains(PRIVATE));
        assert!(!self.body.contains("not-executed.git"));
        match self.envelope.page {
            Page::Discovery(page) => page,
            other => panic!("expected discovery model, got {other:?}"),
        }
    }
}

pub struct Fixture {
    pub database: Database,
    pub public: Vec<i64>,
    pub own_private: i64,
    pub private_org: i64,
    router: Router,
    session: String,
    _root: tempfile::TempDir,
}
impl Fixture {
    pub async fn new(database: Database) -> Self {
        // Explicit IDs make the cross-namespace collision independent of sequence order.
        database.execute("INSERT INTO users(id,username,display_name) VALUES(700000,'discovery_user','User display is not the namespace')", &[]).await;
        database.execute("INSERT INTO organizations(id,name,display_name,billing_email) VALUES(700000,'discovery_org','Organization display is not the namespace','namespace-secret@example.test')", &[]).await;
        let governor =
            db::users::accounts::create_user(database.pool(), "discovery_governor", None)
                .await
                .unwrap();
        database
            .execute(
                "INSERT INTO org_memberships(org_id,user_id,role) VALUES(700000,$1,'owner')",
                &[governor.id],
            )
            .await;
        database.complete_setup().await;
        let session = native::token();
        db::users::sessions::create_session(
            database.pool(),
            &session,
            COLLISION,
            None,
            None,
            chrono::Utc::now() + chrono::Duration::hours(1),
        )
        .await
        .unwrap();
        let mut public = Vec::new();
        for (kind, name) in [
            ("user", "discovery_personal"),
            ("org", "discovery_organization"),
            ("user", "discovery_personal_repeat"),
            ("org", "discovery_organization_repeat"),
        ] {
            let repo = database.insert_repository_row(kind, name, false).await;
            // Issue rows exercise repository-card -> issue-card cache reuse.
            database.execute("INSERT INTO issues(repo_id,number,author_id,title,body) VALUES($1,1,$2,'discovery issue','Fixture')", &[repo, governor.id]).await;
            public.push(repo);
        }
        let own_private = database
            .insert_repository_row("user", "personal_private", true)
            .await;
        let mut private_org = 0;
        // More than every discovery bound: private rows rank ahead by recency/stars.
        // Filtering after LIMIT would starve explore(1), search(50) or home(100).
        for number in 0..101 {
            let repo = database
                .insert_repository_row("org", &format!("discovery_hidden_{number}"), true)
                .await;
            database.execute("UPDATE repositories SET created_at='2099-01-01T00:00:00.000000Z',updated_at='2099-01-01T00:00:00.000000Z',stars_count=999 WHERE id=$1", &[repo]).await;
            database.execute("INSERT INTO issues(repo_id,number,author_id,title,body,updated_at) VALUES($1,1,$2,'discovery hidden issue','Fixture','2099-01-01T00:00:00.000000Z')", &[repo, governor.id]).await;
            private_org = repo;
        }
        let root = tempfile::tempdir().expect("owned discovery objects");
        let state = AppState::new(
            database.pool().clone(),
            root.path().join("repos"),
            Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
            None,
            codeyard::backend::ci::CiConfig::default(),
            None,
        );
        Self {
            database,
            public,
            own_private,
            private_org,
            router: create_router(state),
            session,
            _root: root,
        }
    }
    pub fn missing_git_path(&self) -> std::path::PathBuf {
        self._root.path().join("read-must-not-provision.git")
    }
    pub async fn api(&self, path: &str) -> (StatusCode, serde_json::Value) {
        self.api_as(path, false).await
    }
    pub async fn api_as(&self, path: &str, authenticated: bool) -> (StatusCode, serde_json::Value) {
        let mut request = Request::builder().uri(path);
        if authenticated {
            request = request.header(header::COOKIE, format!("session_id={}", self.session));
        }
        let response = self
            .router
            .clone()
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 2 * 1024 * 1024)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).expect("actual-router JSON response"),
        )
    }

    pub async fn page(&self, path: &str, authenticated: bool) -> Reply {
        let mut request = Request::builder().uri(path).header(
            header::ACCEPT,
            codeyard::webui::bootstrap::MODEL_CONTENT_TYPE,
        );
        if authenticated {
            request = request.header(header::COOKIE, format!("session_id={}", self.session));
        }
        let response = self
            .router
            .clone()
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        assert_eq!(
            response.headers()[header::CACHE_CONTROL],
            "no-store, private"
        );
        assert_eq!(
            response.headers()[header::X_CONTENT_TYPE_OPTIONS],
            "nosniff"
        );
        let bytes = to_bytes(response.into_body(), 2 * 1024 * 1024)
            .await
            .unwrap();
        let body = String::from_utf8(bytes.to_vec()).unwrap();
        let envelope = serde_json::from_str(&body).expect("actual-router safe page model");
        Reply {
            status,
            envelope,
            body,
        }
    }
}
