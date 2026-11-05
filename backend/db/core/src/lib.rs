//! Shared database connections, migrations and record types.

#[macro_export]
macro_rules! native_operation {
    ($postgres:path, $sqlite:path, $(#[$attr:meta])* fn $name:ident($pool:ident: &DbPool $(, $arg:ident: $ty:ty)* $(,)?) -> $result:ty) => {
        #[allow(clippy::too_many_arguments, reason = "The dispatcher repeats its engine functions' signature, and they give the reason.")]
        $(#[$attr])*
        pub async fn $name($pool: &DbPool, $($arg: $ty),*) -> $result {
            match $pool {
                $crate::DbPool::Postgres(native) => $postgres(native, $($arg),*).await,
                $crate::DbPool::Sqlite(native) => $sqlite(native, $($arg),*).await,
            }
        }
    };
}

pub mod connection;
mod migrations;
pub mod models;
pub mod pool;
pub mod postgres;
pub mod sqlite;

pub use connection::{init_db, sqlite_file, DbPool};
pub use migrations::run_migrations;
pub mod ci_admission;
pub mod ci_concurrency;
pub mod ci_concurrency_resolve;
pub mod ci_logs;
pub mod ci_messages;
pub mod ci_policy;
pub mod limits;
pub mod queries;
