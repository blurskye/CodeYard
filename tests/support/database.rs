//! Selects the existing disposable SQLite or seeded PostgreSQL fixture.
use crate::support::{native_database as native, production_ci as production};
use axum::Router;
use codeyard::backend::{
    api::create_router, app::AppState, ci::CiConfig, db, storage::ObjectStore,
};
use std::{
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

pub enum Database {
    Postgres(Box<production::Fixture>),
    Sqlite(native::NativeDatabase),
}
impl Database {
    pub async fn new(pg: bool) -> Self {
        if pg {
            Self::Postgres(Box::new(production::Fixture::new().await))
        } else {
            Self::Sqlite(native::NativeDatabase::new().await)
        }
    }
    pub fn pool(&self) -> &db::DbPool {
        match self {
            Self::Postgres(f) => &f.pool,
            Self::Sqlite(f) => &f.pool,
        }
    }
    /// The PostgreSQL connection pool, for tests that only make sense on PostgreSQL.
    pub fn pg(&self) -> &sqlx::PgPool {
        match self {
            Self::Postgres(f) => &f.raw,
            Self::Sqlite(_) => panic!("this test needs PostgreSQL"),
        }
    }
    /// Runs fixture SQL that is the same on both engines, with numbered id parameters.
    pub async fn execute(&self, sql: &str, ids: &[i64]) {
        match self {
            Self::Postgres(f) => {
                let mut query = sqlx::query(sql);
                for id in ids {
                    query = query.bind(*id);
                }
                query
                    .execute(&f.raw)
                    .await
                    .expect("owned PG fixture mutation");
            }
            Self::Sqlite(f) => {
                let mut query = sqlx::query(sql);
                for id in ids {
                    query = query.bind(*id);
                }
                query
                    .execute(&f.raw)
                    .await
                    .expect("owned SQLite fixture mutation");
            }
        }
    }
    /// The one number a query returns, with numbered id parameters.
    pub async fn scalar(&self, sql: &str, ids: &[i64]) -> i64 {
        match self {
            Self::Postgres(f) => {
                let mut query = sqlx::query_scalar::<_, i64>(sql);
                for id in ids {
                    query = query.bind(*id);
                }
                query.fetch_one(&f.raw).await.expect("owned PG observation")
            }
            Self::Sqlite(f) => {
                let mut query = sqlx::query_scalar::<_, i64>(sql);
                for id in ids {
                    query = query.bind(*id);
                }
                query
                    .fetch_one(&f.raw)
                    .await
                    .expect("owned SQLite observation")
            }
        }
    }
    pub async fn finish(&mut self) {
        match self {
            Self::Postgres(f) => f.finish().await,
            Self::Sqlite(f) => f.close().await,
        }
    }
    pub async fn actor(&self, name: &str) -> i64 {
        db::users::accounts::create_user(self.pool(), name, None)
            .await
            .unwrap()
            .id
    }
    pub async fn count(&self) -> i64 {
        self.scalar("SELECT COUNT(*) FROM repositories", &[]).await
    }
    pub async fn repository(&self, id: i64) -> db::models::Repository {
        let sql = "SELECT * FROM repositories WHERE id=$1";
        match self {
            Self::Postgres(f) => sqlx::query_as(sql)
                .bind(id)
                .fetch_one(&f.raw)
                .await
                .unwrap(),
            Self::Sqlite(f) => sqlx::query_as(sql)
                .bind(id)
                .fetch_one(&f.raw)
                .await
                .unwrap(),
        }
    }
    /// Marks first-run setup as done, so the pages are served instead of the setup form.
    pub async fn complete_setup(&self) {
        db::node_config::upsert_node_configuration(self.pool(), "public", true)
            .await
            .unwrap();
    }
    /// The application router over this database, with repositories and objects under `root`.
    pub async fn router(&self, root: &Path) -> Router {
        router(self.pool(), root).await
    }
}

/// The application router over `pool`, with repositories and objects under `root`.
pub async fn router(pool: &db::DbPool, root: &Path) -> Router {
    create_router(AppState::new(
        pool.clone(),
        root.to_owned(),
        Arc::new(ObjectStore::open(root, None).await.unwrap()),
        None,
        CiConfig::default(),
        None,
    ))
}

/// Waits until at least `waiters` backends are stuck behind the transaction of backend
/// `blocker`, directly or through each other. It reads the server's lock graph, so a sleep
/// never stands in for "the request is waiting".
pub async fn wait_until_blocked(raw: &sqlx::PgPool, blocker: i32, waiters: i64) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let blocked: i64 = sqlx::query_scalar(
            "WITH RECURSIVE waiting(pid) AS (
                 SELECT pid FROM pg_stat_activity WHERE $1 = ANY(pg_blocking_pids(pid))
                 UNION
                 SELECT a.pid FROM pg_stat_activity a JOIN waiting w ON w.pid = ANY(pg_blocking_pids(a.pid))
             ) SELECT COUNT(*) FROM waiting",
        )
        .bind(blocker)
        .fetch_one(raw)
        .await
        .unwrap();
        if blocked >= waiters {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{waiters} backend(s) never waited on the lock held by backend {blocker}; saw {blocked}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}
