use super::{db, native, production};

pub enum Fixture {
    Postgres(Box<production::Fixture>),
    Sqlite(native::NativeDatabase),
}

impl Fixture {
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
    // Canonical owned fixture rows only; no production conversion/recovery.
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
    pub async fn scalar(&self, sql: &str, ids: &[i64]) -> i64 {
        match self {
            Self::Postgres(f) => {
                let mut query = sqlx::query_scalar::<_, i64>(sql);
                for id in ids {
                    query = query.bind(*id);
                }
                query.fetch_one(&f.raw).await.expect("owned PG fixture row")
            }
            Self::Sqlite(f) => {
                let mut query = sqlx::query_scalar::<_, i64>(sql);
                for id in ids {
                    query = query.bind(*id);
                }
                query
                    .fetch_one(&f.raw)
                    .await
                    .expect("owned SQLite fixture row")
            }
        }
    }
    pub async fn actor(&self, name: &str) -> i64 {
        db::users::accounts::create_user(self.pool(), name, None)
            .await
            .unwrap()
            .id
    }
    pub async fn repo(&self, owner: i64, org: Option<i64>) -> i64 {
        if let Some(org) = org {
            self.scalar("INSERT INTO repositories(owner_kind,owner_org_id,name,is_private,disk_path) VALUES('org',$1,'labels',TRUE,'/owned/not-executed.git') RETURNING id", &[org]).await
        } else {
            self.scalar("INSERT INTO repositories(owner_kind,owner_user_id,name,is_private,disk_path) VALUES('user',$1,'labels',TRUE,'/owned/not-executed.git') RETURNING id", &[owner]).await
        }
    }
    pub async fn close(&mut self) {
        match self {
            Self::Postgres(f) => f.finish().await,
            Self::Sqlite(f) => f.close().await,
        }
    }
}
