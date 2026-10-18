//! Database access. Each operation is a named function with a PostgreSQL and a
//! SQLite implementation; `native_operation!` dispatches on the pool type.

macro_rules! native_operation {
    ($postgres:path, $sqlite:path, $(#[$attr:meta])* fn $name:ident($pool:ident: &DbPool $(, $arg:ident: $ty:ty)* $(,)?) -> $result:ty) => {
        $(#[$attr])*
        pub async fn $name($pool: &DbPool, $($arg: $ty),*) -> $result {
            match $pool {
                crate::backend::db::DbPool::Postgres(native) => $postgres(native, $($arg),*).await,
                crate::backend::db::DbPool::Sqlite(native) => $sqlite(native, $($arg),*).await,
            }
        }
    };
}

mod postgres;
mod sqlite;

pub mod activity;
pub mod assignees;
pub mod ci;
pub mod cluster;
mod connection;
pub mod copy;
pub mod discussions;
pub mod files;
pub mod following;
pub mod issues;
pub mod labels;
mod migrations;
pub mod milestones;
pub mod models;
pub mod node_config;
pub mod notifications;
pub mod organizations;
pub mod pull_requests;
pub mod reactions;
pub mod repositories;
pub mod repository_stats;
pub mod secrets;
pub mod timeline;
pub mod users;

pub use connection::{init_db, DbPool};
pub use migrations::run_migrations;
