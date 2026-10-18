//! Disposable native fixtures: the actual router, repository storage and Git initialization.
use super::{database::Database, native};
use axum::{
    body::{to_bytes, Body},
    http::{header, HeaderMap, Request, StatusCode},
    Router,
};
use codeyard::backend::{
    api::create_router,
    app::AppState,
    db::{self, models::OrgRole, organizations as orgs},
    storage::ObjectStore,
};
use codeyard::webui::{
    app::{Page, PageEnvelope},
    pages::discovery as view,
};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use tower::ServiceExt;

pub fn applied<T: std::fmt::Debug>(outcome: orgs::OrganizationOutcome<T>) -> T {
    match outcome {
        orgs::OrganizationOutcome::Applied(value) => value,
        other => panic!("required fixture operation: {other:?}"),
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
    pub fn creation(
        &self,
    ) -> (
        String,
        view::RepositoryCreationChoices,
        view::NewRepositoryForm,
        Option<String>,
    ) {
        let envelope: PageEnvelope =
            serde_json::from_str(&self.body).expect("actual router page model");
        match envelope.page {
            Page::Discovery(view::Page::NewRepository {
                owner,
                choices,
                form,
                error,
                ..
            }) => (owner, choices, form, error.map(|error| error.message)),
            other => panic!("expected creation form: {other:?}"),
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
    pub fn redirect(&self, path: &str) {
        assert_eq!(self.status, StatusCode::SEE_OTHER, "{}", self.body);
        assert_eq!(self.headers[header::LOCATION], path);
        self.secure();
    }
    pub fn no_internal_fields(&self) {
        for field in [
            "disk_path",
            "password_hash",
            "billing_email",
            "secret_encryptor",
            "governance_suspended",
        ] {
            assert!(!self.body.contains(field), "leaked {field}");
        }
    }
}
pub struct Fixture {
    pub database: Database,
    router: Router,
    pub owner: Actor,
    pub admin: Actor,
    pub member: Actor,
    pub outsider: Actor,
    pub staff: Actor,
    pub writer: Actor,
    pub unknown: Actor,
    pub org: i64,
    pub repos_path: PathBuf,
    _root: tempfile::TempDir,
}
impl Fixture {
    pub async fn new(pg: bool) -> Self {
        let database = Database::new(pg).await;
        let pool = database.pool();
        // First org numeric ID belongs to outsider, NOT the organization's actor.
        let outsider = match &database {
            Database::Sqlite(_) => Self::actor(pool, "numeric_collision").await,
            // The existing owned PG fixture already inserted user ID 1.
            Database::Postgres(f) => Self::session(pool, f.owner.id).await,
        };
        let owner = Self::actor(pool, "personal_owner").await;
        let admin = Self::actor(pool, "org_admin").await;
        let member = Self::actor(pool, "ordinary_member").await;
        let staff = Self::actor(pool, "siteadmin_only").await;
        let writer = Self::actor(pool, "direct_repo_writer").await;
        let unknown = Self::actor(pool, "unknown_role").await;
        database
            .execute("UPDATE users SET is_admin=TRUE WHERE id=$1", &[staff.id])
            .await;
        database.execute("INSERT INTO node_configuration(id,setup_completed) VALUES(1,TRUE) ON CONFLICT(id) DO UPDATE SET setup_completed=TRUE", &[]).await;
        let org = Self::organization(pool, owner.id, "canonical_org").await;
        for (actor, role) in [
            (&admin, OrgRole::Admin),
            (&member, OrgRole::Member),
            (&unknown, OrgRole::Member),
        ] {
            applied(
                orgs::add_organization_member(pool, owner.id, org, actor.id, role)
                    .await
                    .unwrap(),
            );
        }
        // Fixture-owned malformed membership; production never repairs it implicitly.
        database
            .execute(
                "UPDATE org_memberships SET role='future_role' WHERE org_id=$1 AND user_id=$2",
                &[org, unknown.id],
            )
            .await;
        assert_eq!(outsider.id, org, "required numeric collision fixture");
        let root = tempfile::tempdir().expect("owned page Git/object storage");
        let repos_path = root.path().join("repos");
        let state = AppState::new(
            pool.clone(),
            repos_path.clone(),
            Arc::new(ObjectStore::open(root.path(), None).await.unwrap()),
            None,
            codeyard::backend::ci::CiConfig::default(),
            None,
        );
        Self {
            database,
            router: create_router(state),
            owner,
            admin,
            member,
            outsider,
            staff,
            writer,
            unknown,
            org,
            repos_path,
            _root: root,
        }
    }
    pub async fn actor(pool: &db::DbPool, username: &str) -> Actor {
        let id = db::users::accounts::create_user(pool, username, None)
            .await
            .unwrap()
            .id;
        Self::session(pool, id).await
    }
    async fn session(pool: &db::DbPool, id: i64) -> Actor {
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
    pub async fn organization(pool: &db::DbPool, actor: i64, slug: &str) -> i64 {
        applied(
            orgs::create_organization(
                pool,
                actor,
                &orgs::NewOrganization::new(
                    slug,
                    orgs::OrganizationProfile::new(None, None).unwrap(),
                )
                .unwrap(),
            )
            .await
            .unwrap(),
        )
        .id
    }
    pub async fn request(
        &self,
        actor: &Actor,
        method: &str,
        path: &str,
        body: String,
        model: bool,
        cross_site: bool,
    ) -> Reply {
        let mut request = Request::builder()
            .method(method)
            .uri(path)
            .header(header::COOKIE, format!("session_id={}", actor.session))
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
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
    pub async fn get(&self, actor: &Actor, path: &str, model: bool) -> Reply {
        self.request(actor, "GET", path, String::new(), model, false)
            .await
    }
    pub fn body(actor: &Actor, fields: &[(&str, &str)]) -> String {
        url::form_urlencoded::Serializer::new(String::new())
            .append_pair("csrf_token", &actor.csrf)
            .extend_pairs(fields.iter().copied())
            .finish()
    }
    pub async fn form(
        &self,
        actor: &Actor,
        path: &str,
        fields: &[(&str, &str)],
        model: bool,
    ) -> Reply {
        self.request(actor, "POST", path, Self::body(actor, fields), model, false)
            .await
    }
    /// Names of everything under the repositories directory. A refused creation
    /// must leave this exactly as it was.
    pub fn disk_inventory(&self) -> Vec<String> {
        if !self.repos_path.exists() {
            return Vec::new();
        }
        let mut entries = std::fs::read_dir(&self.repos_path)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect::<Vec<_>>();
        entries.sort();
        entries
    }
    pub async fn created(
        &self,
        owner: &str,
        name: &str,
        actor: i64,
        org: Option<i64>,
        private: bool,
    ) -> db::models::Repository {
        let repository =
            db::repositories::records::get_repository(self.database.pool(), owner, name)
                .await
                .unwrap()
                .expect("canonical lookup of actual created repo");
        assert_eq!(
            repository.owner_kind,
            if org.is_some() { "org" } else { "user" }
        );
        assert_eq!(
            repository.owner_user_id,
            if org.is_none() { Some(actor) } else { None }
        );
        assert_eq!(repository.owner_org_id, org);
        assert_eq!(repository.is_private, private);
        // Repositories live at <repos>/<uuid>.git, whatever their owner or name.
        let path = Path::new(&repository.disk_path);
        assert_eq!(path.parent(), Some(self.repos_path.as_path()));
        let stem = path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_suffix(".git"))
            .expect("repository directory ends in .git");
        uuid::Uuid::parse_str(stem).expect("repository directory is named by a uuid");
        let git = git2::Repository::open_bare(path).expect("real initialized bare Git");
        // libgit2 1.8.1 is_empty assumes configured default/master, not explicit main.
        assert!(git.is_bare());
        assert_eq!(git.references().unwrap().count(), 0);
        assert!(matches!(git.head(), Err(error) if error.code() == git2::ErrorCode::UnbornBranch));
        assert_eq!(
            git.find_reference("HEAD").unwrap().symbolic_target(),
            Some("refs/heads/main")
        );
        repository
    }
}
