#![allow(dead_code)]

// git_streaming is included only by git/remote.rs, which it reaches through super::.
// openssh_daemon and openssh_gateway are not included by any test binary.
pub mod database;
pub mod fake_webui_bundle;
pub mod git_remote;
pub mod git_server;
pub mod native_database;
pub mod production_ci;
pub mod pull_requests;
pub mod rpc_operations;

/// Password hashing has a few process-wide permits, and a request that finds them taken fails at
/// once. Tests in one binary that hash passwords hold this lock so they do not compete.
pub static PASSWORD_WORK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
