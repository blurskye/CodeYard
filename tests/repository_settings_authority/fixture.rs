use super::{database::Database, db};
use serde::Serialize;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};
use uuid::Uuid;

pub struct Fixture {
    pub database: Database,
    storage: tempfile::TempDir,
}

// Includes the visibility lock, which the public Repository read projection omits.
#[derive(sqlx::FromRow, Serialize)]
struct RepositorySnapshot {
    #[sqlx(flatten)]
    repository: db::models::Repository,
    visibility_locked: bool,
}

impl Fixture {
    pub async fn new(pg: bool) -> Self {
        Self {
            database: Database::new(pg).await,
            storage: tempfile::tempdir().expect("owned repository storage"),
        }
    }
    pub async fn actor(&self, name: &str) -> i64 {
        db::users::accounts::create_user(self.database.pool(), name, None)
            .await
            .unwrap()
            .id
    }
    pub async fn repo(&self, owner: i64, org: Option<i64>, name: &str) -> i64 {
        let path = self
            .storage
            .path()
            .join(format!("{}.git", Uuid::new_v4().simple()));
        git2::Repository::init_bare(&path).expect("owned bare Git data");
        std::fs::write(
            path.join("retained-marker"),
            b"original repository content\0",
        )
        .unwrap();
        let repo = db::repositories::records::create_repository(
            self.database.pool(),
            owner,
            name,
            Some("original"),
            true,
            path.to_str().unwrap(),
        )
        .await
        .unwrap()
        .id;
        if let Some(org) = org {
            self.database.execute("UPDATE repositories SET owner_kind='org',owner_user_id=NULL,owner_org_id=$1 WHERE id=$2", &[org, repo]).await;
        }
        repo
    }
    /// The whole row as JSON, or `null` once the repository has been deleted.
    pub async fn snapshot(&self, repo: i64) -> Vec<u8> {
        let sql = "SELECT * FROM repositories WHERE id=$1";
        let row: Option<RepositorySnapshot> = match &self.database {
            Database::Postgres(f) => sqlx::query_as(sql)
                .bind(repo)
                .fetch_optional(&f.raw)
                .await
                .unwrap(),
            Database::Sqlite(f) => sqlx::query_as(sql)
                .bind(repo)
                .fetch_optional(&f.raw)
                .await
                .unwrap(),
        };
        serde_json::to_vec(&row).unwrap()
    }
    pub fn files(&self) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
        fn visit(root: &Path, path: &Path, entries: &mut BTreeMap<PathBuf, Option<Vec<u8>>>) {
            for entry in std::fs::read_dir(path).unwrap() {
                let path = entry.unwrap().path();
                let key = path.strip_prefix(root).unwrap().to_path_buf();
                if path.is_dir() {
                    entries.insert(key, None);
                    visit(root, &path, entries);
                } else {
                    entries.insert(key, Some(std::fs::read(path).unwrap()));
                }
            }
        }
        let mut entries = BTreeMap::new();
        visit(self.storage.path(), self.storage.path(), &mut entries);
        entries
    }
    // Malformed historical identity exists ONLY in this owned disposable fixture.
    // No application schema, authority helper or conversion path is weakened.
    pub async fn historical_unknown_kind(&self, repo: i64) {
        match &self.database {
            Database::Postgres(f) => {
                let mut tx = f.raw.begin().await.unwrap();
                sqlx::query("ALTER TABLE repositories DROP CONSTRAINT IF EXISTS repo_owner_check")
                    .execute(&mut *tx)
                    .await
                    .unwrap();
                sqlx::query("UPDATE repositories SET owner_kind='unknown' WHERE id=$1")
                    .bind(repo)
                    .execute(&mut *tx)
                    .await
                    .unwrap();
                tx.commit().await.unwrap();
            }
            Database::Sqlite(f) => {
                let mut tx = f.raw.begin_with("BEGIN IMMEDIATE").await.unwrap();
                sqlx::query("PRAGMA ignore_check_constraints=ON")
                    .execute(&mut *tx)
                    .await
                    .unwrap();
                sqlx::query("UPDATE repositories SET owner_kind='unknown' WHERE id=$1")
                    .bind(repo)
                    .execute(&mut *tx)
                    .await
                    .unwrap();
                sqlx::query("PRAGMA ignore_check_constraints=OFF")
                    .execute(&mut *tx)
                    .await
                    .unwrap();
                tx.commit().await.unwrap();
            }
        }
    }
}
