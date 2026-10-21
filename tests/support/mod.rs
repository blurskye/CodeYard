#![allow(dead_code)]

pub mod database;
pub mod fake_webui_bundle;
pub mod git_remote;
pub mod git_server;
pub mod http_client;
pub mod native_database;
pub mod organizations;
pub mod production_ci;
pub mod rpc_operations;
pub mod server;

/// Password hashing has a few process-wide permits, and a request that finds them taken fails at
/// once. Tests in one binary that hash passwords hold this lock so they do not compete.
pub static PASSWORD_WORK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
