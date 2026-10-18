//! Selects the existing disposable SQLite or seeded PostgreSQL fixture.
use crate::support::{native_database as native, production_ci as production};
use codeyard::backend::db;

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
        let sql = "SELECT COUNT(*) FROM repositories";
        match self {
            Self::Postgres(f) => sqlx::query_scalar(sql).fetch_one(&f.raw).await.unwrap(),
            Self::Sqlite(f) => sqlx::query_scalar(sql).fetch_one(&f.raw).await.unwrap(),
        }
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
}
