use codeyard_app::app::AppState;
use std::{path::PathBuf, sync::Arc};
pub async fn closed_state() -> AppState {
    // A closed lazy pool can't reach any server or change any data.
    let pool = sqlx::postgres::PgPoolOptions::new()
        .connect_lazy("postgresql://fixture:fixture@127.0.0.1:1/ssr_fixture")
        .map(codeyard_db_core::pool::Pool::from)
        .unwrap();
    pool.close().await;
    let object_root = tempfile::tempdir().unwrap();
    let storage = Arc::new(
        codeyard_storage::ObjectStore::open(object_root.path(), None)
            .await
            .unwrap(),
    );
    AppState::new(
        codeyard_db_core::DbPool::from(pool),
        PathBuf::from("/unused-ssr-fixture"),
        storage,
        None,
        codeyard_ci::config::CiConfig::default(),
        None,
    )
}
