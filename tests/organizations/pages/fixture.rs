//! Shared disposable fixtures only; no server listeners, executor or ambient installation.
use super::{database::Database, native};
use axum::{
    body::{to_bytes, Body},
    http::{header, HeaderMap, Request, StatusCode},
    Router,
};
use codeyard::{
    backend::{
        api::create_router,
        app::AppState,
        db::{self, organizations::*},
        storage::ObjectStore,
    },
    webui::{
        app::{Page, PageEnvelope},
        pages::orgs as view,
    },
};
use std::sync::Arc;
use tower::ServiceExt;

pub const INDEX: &str = "/settings/organizations";
impl Database {
    pub async fn count_rows(&self, sql: &str, id: i64) -> i64 {
        match self {
            Self::Sqlite(db) => sqlx::query_scalar(sql)
                .bind(id)
                .fetch_one(&db.raw)
                .await
                .unwrap(),
            Self::Postgres(db) => sqlx::query_scalar(sql)
                .bind(id)
                .fetch_one(&db.raw)
                .await
                .unwrap(),
        }
    }
    pub async fn audit_metadata(&self, org: i64) -> serde_json::Value {
        let sql = "SELECT CAST(metadata AS TEXT) FROM audit_log WHERE org_id=$1 AND action='org.governance.recover_owner'";
        let value: String = match self {
            Self::Sqlite(db) => sqlx::query_scalar(sql)
                .bind(org)
                .fetch_one(&db.raw)
                .await
                .unwrap(),
            Self::Postgres(db) => sqlx::query_scalar(sql)
                .bind(org)
                .fetch_one(&db.raw)
                .await
                .unwrap(),
        };
        serde_json::from_str(&value).unwrap()
    }
    pub async fn organization_repository(&self, org: i64, name: &str, private: bool) -> i64 {
        // Fixture-only direct organization row: not personal-to-org conversion or
        // a production storage reservation/creation implementation.
        let sql = "INSERT INTO repositories(owner_kind,owner_org_id,name,is_private,disk_path) VALUES('org',$1,$2,$3,'/fixture/not-executed.git') RETURNING id";
        match self {
            Self::Sqlite(db) => sqlx::query_scalar(sql)
                .bind(org)
                .bind(name)
                .bind(private)
                .fetch_one(&db.raw)
                .await
                .unwrap(),
            Self::Postgres(db) => sqlx::query_scalar(sql)
                .bind(org)
                .bind(name)
                .bind(private)
                .fetch_one(&db.raw)
                .await
                .unwrap(),
        }
    }
}
pub struct Actor {
    pub id: i64,
    pub session: String,
    pub csrf: String,
}
pub struct Reply {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: String,
}
impl Reply {
    pub fn envelope(&self) -> PageEnvelope {
        serde_json::from_str(&self.body).expect("authorized real router model")
    }
    pub fn organization(&self) -> view::Page {
        match self.envelope().page {
            Page::Organizations(page) => page,
            other => panic!("expected organization page, got {other:?}"),
        }
    }
    pub fn error_message(&self) -> String {
        match self.envelope().page {
            Page::Error { message, .. } => message,
            other => panic!("expected error, got {other:?}"),
        }
    }
    pub fn secure(&self) {
        assert_eq!(self.headers[header::CACHE_CONTROL], "no-store, private");
        assert_eq!(self.headers[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
        assert!(self.headers.contains_key(header::CONTENT_SECURITY_POLICY));
        assert!(self
            .headers
            .get_all(header::VARY)
            .iter()
            .any(|v| v.to_str().unwrap().contains("Cookie")));
    }
    #[track_caller]
    pub fn redirect(&self, path: &str) {
        assert_eq!(self.status, StatusCode::SEE_OTHER);
        assert_eq!(self.headers[header::LOCATION], path);
        self.secure();
    }
}
pub struct Fixture {
    pub database: Database,
    pub router: Router,
    pub owner: Actor,
    pub member: Actor,
    pub outsider: Actor,
    pub org: i64,
    _root: tempfile::TempDir,
}
impl Fixture {
    pub async fn new(database: Database) -> Self {
        let pool = database.pool();
        // The account DAO accepts `orgs`, which HTTP username admission rejects.
        let owner = Self::actor(pool, "orgs").await;
        let member = Self::actor(pool, "ordinary_member").await;
        let outsider = Self::actor(pool, "site_admin_outsider").await;
        database
            .execute("UPDATE users SET is_admin=TRUE WHERE id=$1", &[outsider.id])
            .await;
        database.execute("INSERT INTO node_configuration(id,setup_completed) VALUES(1,TRUE) ON CONFLICT(id) DO UPDATE SET setup_completed=TRUE", &[]).await;
        let org = applied(
            create_organization(
                pool,
                owner.id,
                &NewOrganization::new(
                    "native_org",
                    OrganizationProfile::new(
                        Some("Stored display"),
                        Some("Stored description\nSecond line"),
                    )
                    .unwrap(),
                )
                .unwrap(),
            )
            .await
            .unwrap(),
        )
        .id;
        applied(
            add_organization_member(pool, owner.id, org, member.id, db::models::OrgRole::Member)
                .await
                .unwrap(),
        );
        let root = tempfile::tempdir().expect("owned organization page Git/objects");
        let path = root.path().join("pages.git");
        let git = git2::Repository::init_bare(&path).unwrap();
        let tree_id = git.treebuilder(None).unwrap().write().unwrap();
        let tree = git.find_tree(tree_id).unwrap();
        let signature = git2::Signature::now("Fixture", "fixture@example.test").unwrap();
        git.commit(
            Some("refs/heads/main"),
            &signature,
            &signature,
            "fixture",
            &tree,
            &[],
        )
        .unwrap();
        git.set_head("refs/heads/main").unwrap();
        git.branch(
            "feature/responsive-toolbar",
            &git.head().unwrap().peel_to_commit().unwrap(),
            false,
        )
        .unwrap();
        db::repositories::records::create_repository(
            pool,
            owner.id,
            "repo",
            None,
            false,
            path.to_str().unwrap(),
        )
        .await
        .unwrap();
        let state = AppState::new(
            pool.clone(),
            root.path().join("repos"),
            Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
            None,
            codeyard::backend::ci::CiConfig::default(),
            None,
        );
        drop(tree);
        drop(git);
        Self {
            database,
            router: create_router(state),
            owner,
            member,
            outsider,
            org,
            _root: root,
        }
    }
    pub fn path(&self) -> String {
        format!("{INDEX}/{}", self.org)
    }
    pub async fn actor(pool: &db::DbPool, username: &str) -> Actor {
        let id = db::users::accounts::create_user(pool, username, None)
            .await
            .unwrap()
            .id;
        let session = native::token();
        let csrf = native::token();
        db::users::sessions::create_session(
            pool,
            &session,
            id,
            None,
            None,
            chrono::Utc::now() + chrono::Duration::hours(1),
        )
        .await
        .unwrap();
        assert_eq!(
            db::users::sessions::get_or_create_csrf_token(pool, &session, Some(id), &csrf, 3600)
                .await
                .unwrap(),
            Some(csrf.clone())
        );
        Actor { id, session, csrf }
    }
    pub async fn request(
        &self,
        actor: &Actor,
        method: &str,
        path: &str,
        body: String,
        content_type: &str,
        flags: (bool, bool),
    ) -> Reply {
        let mut request = Request::builder()
            .method(method)
            .uri(path)
            .header(header::COOKIE, format!("session_id={}", actor.session))
            .header(header::CONTENT_TYPE, content_type);
        let (model, cross_site) = flags;
        if model {
            request = request.header(
                header::ACCEPT,
                codeyard::webui::bootstrap::MODEL_CONTENT_TYPE,
            );
        }
        if cross_site {
            request = request.header("sec-fetch-site", "cross-site");
        }
        let response = self
            .router
            .clone()
            .oneshot(request.body(Body::from(body)).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let body = String::from_utf8(
            to_bytes(response.into_body(), 2 * 1024 * 1024)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap();
        Reply {
            status,
            headers,
            body,
        }
    }
    pub async fn get(&self, actor: &Actor, path: &str) -> Reply {
        self.request(
            actor,
            "GET",
            path,
            String::new(),
            "application/x-www-form-urlencoded",
            (true, false),
        )
        .await
    }
    pub async fn form(&self, actor: &Actor, path: &str, fields: &[(&str, &str)]) -> Reply {
        let body = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("csrf_token", &actor.csrf)
            .extend_pairs(fields.iter().copied())
            .finish();
        self.request(
            actor,
            "POST",
            path,
            body,
            "application/x-www-form-urlencoded",
            (true, false),
        )
        .await
    }
    pub async fn team(&self, slug: &str, privacy: TeamPrivacy) -> i64 {
        applied(
            create_organization_team(
                self.database.pool(),
                self.owner.id,
                self.org,
                &NewTeam::new(slug, TeamProfile::new(slug, None, privacy).unwrap()).unwrap(),
            )
            .await
            .unwrap(),
        )
        .id
    }
}
pub fn applied<T: std::fmt::Debug>(outcome: OrganizationOutcome<T>) -> T {
    match outcome {
        OrganizationOutcome::Applied(value) => value,
        other => panic!("required fixture operation: {other:?}"),
    }
}
