use super::{database::Database, db};

// Test-only raw persistence snapshot, never an application/HTTP projection.
#[derive(Debug, PartialEq, sqlx::FromRow)]
pub struct SecretSnapshot {
    pub id: i64,
    pub repo_id: i64,
    pub name: String,
    pub encrypted_value: Vec<u8>,
    pub nonce: Vec<u8>,
    pub created_by_id: Option<i64>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}
impl Database {
    pub async fn repo(&self, owner: i64, org: Option<i64>, name: &str, private: bool) -> i64 {
        let repo = db::repositories::records::create_repository(
            self.pool(),
            owner,
            name,
            None,
            private,
            "/owned/not-executed.git",
        )
        .await
        .unwrap()
        .id;
        if let Some(org) = org {
            self.execute("UPDATE repositories SET owner_kind='org',owner_user_id=NULL,owner_org_id=$1 WHERE id=$2", &[org, repo]).await;
        }
        repo
    }
    pub async fn snapshot(&self, repo: i64) -> Vec<SecretSnapshot> {
        let sql = "SELECT id,repo_id,name,encrypted_value,nonce,created_by_id,created_at,updated_at FROM repo_secrets WHERE repo_id=$1 ORDER BY name";
        match self {
            Self::Postgres(f) => sqlx::query_as(sql)
                .bind(repo)
                .fetch_all(&f.raw)
                .await
                .unwrap(),
            Self::Sqlite(f) => sqlx::query_as(sql)
                .bind(repo)
                .fetch_all(&f.raw)
                .await
                .unwrap(),
        }
    }
}
