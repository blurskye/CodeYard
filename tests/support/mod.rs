#![allow(
    dead_code,
    reason = "Each test binary uses its own part of the shared support code."
)]

pub mod database;
pub mod fake_webui_bundle;
pub mod git_remote;
pub mod git_server;
pub mod github_spec;
pub mod http_client;
pub mod native_database;
pub mod organizations;
pub mod production_ci;
pub mod rpc_operations;
pub mod server;

/// Password hashing has a few process-wide permits, and a request that finds them taken fails at
/// once. Tests in one binary that hash passwords hold this lock so they do not compete.
pub static PASSWORD_WORK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

pub async fn live_session(pool: &codeyard::backend::db::DbPool, actor: i64) -> String {
    let id = uuid::Uuid::new_v4().simple().to_string().repeat(2);
    codeyard::backend::db::users::sessions::create_session(
        pool,
        &id,
        actor,
        None,
        None,
        chrono::Utc::now() + chrono::Duration::hours(1),
    )
    .await
    .unwrap();
    id
}
