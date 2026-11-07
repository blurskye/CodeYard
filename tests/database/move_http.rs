//! The real HTTP write path must obey a move's database freeze, including rollback.
use crate::support::native_database::PostgresDatabase;
use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
    Router,
};
use codeyard_db_cluster::moves::{self, Account};
use codeyard_model::cells::ObjectKind;
use serde_json::{json, Value};
use std::{sync::Arc, time::Duration};
use tower::ServiceExt;

async fn request(
    app: &Router,
    token: &str,
    method: &str,
    path: &str,
    body: Value,
) -> (StatusCode, Value) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

async fn token(database: &PostgresDatabase, user: i64) -> String {
    let session = codeyard_identity::sessions::fresh_id(&database.pool).unwrap();
    codeyard_db_identity::users::sessions::create_session(
        &database.pool,
        &session,
        user,
        None,
        None,
        chrono::Utc::now() + chrono::Duration::hours(1),
    )
    .await
    .unwrap();
    let token = database
        .pool
        .cell_prefix()
        .token(
            codeyard_support::tokens::PAT_PREFIX,
            &uuid::Uuid::new_v4().simple().to_string().repeat(2),
        )
        .unwrap();
    codeyard_db_identity::users::tokens::create_token(
        &database.pool,
        user,
        &session,
        "moves",
        &codeyard_support::token_hash::token_hash(&token),
        "write",
        None,
    )
    .await
    .unwrap()
    .unwrap();
    token
}

async fn app(database: &PostgresDatabase, root: &std::path::Path) -> Router {
    codeyard_db_cluster::node_config::upsert_node_configuration(&database.pool, "public", true)
        .await
        .unwrap();
    let store = Arc::new(
        codeyard_storage::ObjectStore::open(root, None)
            .await
            .unwrap(),
    );
    codeyard_api_rest::create_router(codeyard_app::app::AppState::new(
        database.pool.clone(),
        root.into(),
        store,
        None,
        Default::default(),
        None,
    ))
}

#[tokio::test]
async fn http_writes_obey_the_freeze_and_resume_after_a_cancel() {
    crate::support::fake_webui_bundle::install();
    let mut database = PostgresDatabase::new().await;
    let user = codeyard_db_identity::users::accounts::create_user(&database.pool, "moving", None)
        .await
        .unwrap();
    let token = token(&database, user.id).await;
    let root = tempfile::tempdir().unwrap();
    let repo = root.path().join("repo.git");
    git2::Repository::init_bare(&repo).unwrap();
    codeyard_db_repos::repositories::records::create_repository(
        &database.pool,
        user.id,
        "repo",
        None,
        false,
        repo.to_str().unwrap(),
    )
    .await
    .unwrap();
    let app = app(&database, root.path()).await;
    let path = "/api/v3/repos/moving/repo/issues";
    let (status, issue) =
        request(&app, &token, "POST", path, json!({"title":"Before freeze"})).await;
    assert_eq!(status, StatusCode::CREATED, "{issue}");
    let account = Account::new(ObjectKind::User, user.id).unwrap();
    let wait = Duration::from_secs(10);
    moves::begin(&database.raw, account, 2, wait).await.unwrap();
    moves::freeze(&database.raw, account, Duration::from_secs(60), wait)
        .await
        .unwrap();
    let (status, refusal) =
        request(&app, &token, "POST", path, json!({"title":"Must not land"})).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{refusal}");
    let (status, issues) = request(&app, &token, "GET", path, Value::Null).await;
    assert_eq!(status, StatusCode::OK, "{issues}");
    assert_eq!(issues.as_array().unwrap().len(), 1, "{issues}");
    assert!(moves::cancel(&database.raw, account).await.unwrap());
    let (status, after) =
        request(&app, &token, "POST", path, json!({"title":"After cancel"})).await;
    assert_eq!(status, StatusCode::CREATED, "{after}");
    let (status, issues) = request(&app, &token, "GET", path, Value::Null).await;
    assert_eq!(status, StatusCode::OK, "{issues}");
    assert_eq!(issues.as_array().unwrap().len(), 2, "{issues}");
    database.finish().await;
}

#[tokio::test]
async fn an_organization_with_a_thousand_members_moves_without_restoring_a_revoked_members_access()
{
    crate::support::fake_webui_bundle::install();
    let mut source = PostgresDatabase::new().await;
    let mut target = PostgresDatabase::new().await;
    for (database, prefix) in [(&source, 1), (&target, 2)] {
        codeyard_db_core::cells::initialize(
            &database.pool,
            codeyard_support::cells::Prefix::new(prefix),
        )
        .await
        .unwrap();
        codeyard_db_core::cells::load(&database.pool).await.unwrap();
    }
    let owner = codeyard_db_identity::users::accounts::create_user(&source.pool, "owner", None)
        .await
        .unwrap();
    let organization: i64 = sqlx::query_scalar(
        "INSERT INTO organizations(name,home_cell) VALUES('acme',1) RETURNING id",
    )
    .fetch_one(&source.raw)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO organization_settings(org_id,default_repository_permission) VALUES($1,1)",
    )
    .bind(organization)
    .execute(&source.raw)
    .await
    .unwrap();
    sqlx::query("INSERT INTO org_memberships(org_id,user_id,role) VALUES($1,$2,'owner')")
        .bind(organization)
        .bind(owner.id)
        .execute(&source.raw)
        .await
        .unwrap();
    let members: Vec<i64> = sqlx::query_scalar("INSERT INTO users(username,home_cell) SELECT 'member'||n,2 FROM generate_series(1,999) n RETURNING id")
        .fetch_all(&target.raw).await.unwrap();
    // These members are native at the destination already. The source holds only stubs.
    sqlx::query("INSERT INTO users(id,username,home_cell,is_stub) SELECT id,'member'||id,2,TRUE FROM unnest($1::bigint[]) id")
        .bind(&members).execute(&source.raw).await.unwrap();
    sqlx::query(
        "INSERT INTO org_memberships(org_id,user_id) SELECT $1,id FROM unnest($2::bigint[]) id",
    )
    .bind(organization)
    .bind(&members)
    .execute(&source.raw)
    .await
    .unwrap();
    let root = tempfile::tempdir().unwrap();
    let git = root.path().join("repo.git");
    git2::Repository::init_bare(&git).unwrap();
    let repository: i64 = sqlx::query_scalar("INSERT INTO repositories(owner_kind,owner_org_id,name,disk_path,home_cell,is_private) VALUES('org',$1,'private',$2,1,TRUE) RETURNING id")
        .bind(organization).bind(git.to_str().unwrap()).fetch_one(&source.raw).await.unwrap();
    let kept = token(&target, members[0]).await;
    let revoked = token(&target, members[998]).await;
    let account = Account::new(ObjectKind::Organization, organization).unwrap();
    let wait = Duration::from_secs(30);
    moves::begin(&source.raw, account, 2, wait).await.unwrap();
    sqlx::query("DELETE FROM org_memberships WHERE org_id=$1 AND user_id=$2")
        .bind(organization)
        .bind(members[998])
        .execute(&source.raw)
        .await
        .unwrap();
    // The source's pointer notices are delivered by the real outbox worker in production.
    // This data/HTTP fixture has no peer listeners; acknowledge only its membership hints.
    sqlx::query("UPDATE cell_outbox SET delivered_at=clock_timestamp() WHERE topic='pointer'")
        .execute(&source.raw)
        .await
        .unwrap();
    moves::freeze(&source.raw, account, Duration::from_secs(300), wait)
        .await
        .unwrap();
    let paths =
        std::collections::BTreeMap::from([(repository, git.to_string_lossy().into_owned())]);
    moves::copy(&source.raw, &target.raw, account, 2, &paths)
        .await
        .unwrap();
    moves::switch(&source.raw, account).await.unwrap();
    moves::activate(&source.raw, &target.raw, account)
        .await
        .unwrap();
    moves::release(&source.raw, account).await.unwrap();
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM org_memberships WHERE org_id=$1")
        .bind(organization)
        .fetch_one(&target.raw)
        .await
        .unwrap();
    assert_eq!(count, 999);
    let app = app(&target, root.path()).await;
    let path = "/api/v3/repos/acme/private/issues";
    let (status, body) = request(&app, &kept, "GET", path, Value::Null).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = request(&app, &revoked, "GET", path, Value::Null).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    source.finish().await;
    target.finish().await;
}
