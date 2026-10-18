use super::{db, native, production};
use serde_json::{json, Value};

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
    pub async fn repo(&self, owner: i64, org: Option<i64>, name: &str) -> i64 {
        let repo = db::repositories::records::create_repository(
            self.pool(),
            owner,
            name,
            None,
            true,
            "/owned/not-executed.git",
        )
        .await
        .unwrap()
        .id;
        if let Some(org) = org {
            self.execute(
                "UPDATE repositories SET owner_kind='org',owner_user_id=NULL,owner_org_id=$1 WHERE id=$2",
                &[org, repo],
            ).await;
        }
        repo
    }
    pub async fn notify(&self, recipient: i64, repo: Option<i64>, title: &str) -> i64 {
        let id = db::notifications::records::create_notification(
            self.pool(),
            db::notifications::records::NewNotification {
                user_id: recipient,
                actor_id: None,
                repo_id: repo,
                kind: if repo.is_some() { "issue" } else { "account" },
                title,
                body: Some(title),
                link: Some(title),
                metadata: &json!({"private_marker": title}),
            },
        )
        .await
        .unwrap()
        .id;
        // Equal persisted timestamps exercise stable id ordering across pages.
        self.execute(
            "UPDATE notifications SET created_at='2026-01-01T00:00:00.000000Z' WHERE id=$1",
            &[id],
        )
        .await;
        id
    }
    pub async fn history(&self, recipient: i64) -> Vec<Value> {
        type Stored = (
            i64,
            Option<i64>,
            String,
            Option<String>,
            Option<String>,
            String,
            Option<String>,
            Option<i64>,
            i64,
            Option<i64>,
            String,
            String,
        );
        let sql = "SELECT id,repo_id,title,body,link,CAST(metadata AS TEXT),CAST(read_at AS TEXT),scope_repository_id,user_id,actor_id,kind,CAST(created_at AS TEXT) FROM notifications WHERE user_id=$1 ORDER BY id DESC";
        let rows: Vec<Stored> = match self {
            Self::Postgres(f) => sqlx::query_as(sql)
                .bind(recipient)
                .fetch_all(&f.raw)
                .await
                .unwrap(),
            Self::Sqlite(f) => sqlx::query_as(sql)
                .bind(recipient)
                .fetch_all(&f.raw)
                .await
                .unwrap(),
        };
        rows.into_iter()
            .map(|r| {
                json!({
                    "id": r.0, "repo": r.1, "title": r.2, "body": r.3,
                    "link": r.4, "metadata": r.5, "read_at": r.6, "scope": r.7,
                    "recipient": r.8, "actor": r.9, "kind": r.10, "created_at": r.11,
                })
            })
            .collect()
    }
    // Owned corruption ONLY in this disposable migrated schema/file.
    // Never install a production bypass or edit the schema files.
    pub async fn corrupt(&self, sql: &str, ids: &[i64]) {
        match self {
            Self::Postgres(f) => {
                let mut tx = f.raw.begin().await.unwrap();
                for constraint in [
                    "repo_owner_check",
                    "repositories_owner_user_id_fkey",
                    "repositories_owner_org_id_fkey",
                ] {
                    sqlx::query(&format!(
                        "ALTER TABLE repositories DROP CONSTRAINT IF EXISTS {constraint}"
                    ))
                    .execute(&mut *tx)
                    .await
                    .unwrap();
                }
                sqlx::query("ALTER TABLE notifications DROP CONSTRAINT IF EXISTS notifications_repo_id_fkey")
                    .execute(&mut *tx).await.unwrap();
                let mut query = sqlx::query(sql);
                for id in ids {
                    query = query.bind(*id);
                }
                query.execute(&mut *tx).await.unwrap();
                tx.commit().await.unwrap();
            }
            Self::Sqlite(f) => {
                let mut connection = f.raw.acquire().await.unwrap();
                sqlx::query("PRAGMA foreign_keys=OFF")
                    .execute(&mut *connection)
                    .await
                    .unwrap();
                sqlx::query("PRAGMA ignore_check_constraints=ON")
                    .execute(&mut *connection)
                    .await
                    .unwrap();
                let mut query = sqlx::query(sql);
                for id in ids {
                    query = query.bind(*id);
                }
                query.execute(&mut *connection).await.unwrap();
                sqlx::query("PRAGMA ignore_check_constraints=OFF")
                    .execute(&mut *connection)
                    .await
                    .unwrap();
                sqlx::query("PRAGMA foreign_keys=ON")
                    .execute(&mut *connection)
                    .await
                    .unwrap();
            }
        }
    }
    pub async fn rejected(&self, sql: &str, ids: &[i64]) {
        let error = match self {
            Self::Postgres(f) => {
                let mut query = sqlx::query(sql);
                for id in ids {
                    query = query.bind(*id);
                }
                query
                    .execute(&f.raw)
                    .await
                    .expect_err("notification constraint must reject")
            }
            Self::Sqlite(f) => {
                let mut query = sqlx::query(sql);
                for id in ids {
                    query = query.bind(*id);
                }
                query
                    .execute(&f.raw)
                    .await
                    .expect_err("notification constraint must reject")
            }
        };
        let code = error
            .as_database_error()
            .and_then(|e| e.code())
            .expect("constraint error code");
        assert!(
            matches!(code.as_ref(), "23502" | "23514" | "275" | "1299" | "1811"),
            "expected not-null/check/trigger rejection, not an unrelated SQL failure"
        );
    }
    pub async fn reopen(&mut self) {
        match self {
            Self::Postgres(f) => {
                let options = (*f.raw.connect_options()).clone();
                f.pool.close().await;
                f.raw.close().await;
                f.raw = sqlx::postgres::PgPoolOptions::new()
                    .connect_with(options)
                    .await
                    .unwrap();
                f.pool = f.raw.clone().into();
                db::run_migrations(&f.pool)
                    .await
                    .expect("verify reopened PostgreSQL history");
            }
            Self::Sqlite(f) => f.reopen().await,
        }
    }
    pub async fn close(&mut self) {
        match self {
            Self::Postgres(f) => {
                f.pool.close().await;
                f.raw.close().await;
                f.finish().await;
            }
            Self::Sqlite(f) => f.close().await,
        }
    }
}
