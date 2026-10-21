//! Disposable native-database fixtures. Raw drivers are test-only, never exposed
//! through the application handle. No environment mutation, .env or silent skip.
use codeyard::backend::db;
use sqlx::{
    postgres::PgPoolOptions,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous},
};
use std::{str::FromStr, time::Duration};
use uuid::Uuid;

pub struct NativeDatabase {
    pub pool: db::DbPool,
    pub raw: sqlx::SqlitePool,
    pub url: String,
    _directory: tempfile::TempDir,
}

impl NativeDatabase {
    pub fn installation_directory(&self) -> &std::path::Path {
        self._directory.path()
    }
    pub async fn new() -> Self {
        let directory = tempfile::tempdir().expect("owned SQLite fixture directory");
        let url = format!(
            "sqlite://{}",
            directory.path().join("application.sqlite").display()
        );
        let pool = db::init_db(&url).await.expect("native SQLite open");
        db::run_migrations(&pool)
            .await
            .expect("native SQLite migration");
        let options = SqliteConnectOptions::from_str(&url)
            .unwrap()
            .with_regexp()
            .foreign_keys(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Full)
            .busy_timeout(Duration::from_secs(3));
        let raw = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(options)
            .await
            .unwrap();
        Self {
            pool,
            raw,
            url,
            _directory: directory,
        }
    }

    pub async fn second_handle(&self) -> db::DbPool {
        db::init_db(&self.url)
            .await
            .expect("independent selected database handle")
    }

    pub async fn reopen(&mut self) {
        self.pool.close().await;
        self.raw.close().await;
        self.pool = db::init_db(&self.url)
            .await
            .expect("reopen persistent application database");
        db::run_migrations(&self.pool)
            .await
            .expect("verify independent SQLite history");
        self.raw = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(
                SqliteConnectOptions::from_str(&self.url)
                    .unwrap()
                    .with_regexp()
                    .foreign_keys(true)
                    .busy_timeout(Duration::from_secs(3)),
            )
            .await
            .unwrap();
    }

    pub async fn close(&self) {
        self.pool.close().await;
        self.raw.close().await;
    }
}

pub struct PostgresDatabase {
    pub pool: db::DbPool,
    pub raw: sqlx::PgPool,
    pub url: String,
    pub schema: String,
    removed: bool,
}

impl PostgresDatabase {
    /// The URL a server under test connects with. Its sessions carry the schema name as their
    /// application name, so the cleanup can end any that outlive the test.
    pub fn connection_url(&self) -> String {
        format!(
            "{}?options=-csearch_path%3D{schema}%20-capplication_name%3D{schema}",
            self.url,
            schema = self.schema
        )
    }

    pub async fn new() -> Self {
        let url = std::env::var("GITSERV_TEST_DATABASE_URL")
            .expect("set GITSERV_TEST_DATABASE_URL to a PostgreSQL URL");
        let parsed = url::Url::parse(&url).expect("fixture URL");
        assert!(
            matches!(parsed.scheme(), "postgres" | "postgresql")
                && parsed.path() == "/gitserv_test"
                && parsed.query().is_none()
                && parsed.fragment().is_none(),
            "tests only run against a database named gitserv_test"
        );
        let admin = PgPoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await
            .expect("required PostgreSQL fixture");
        let schema = format!("test_{}", Uuid::new_v4().simple());
        sqlx::query(&format!("CREATE SCHEMA \"{schema}\""))
            .execute(&admin)
            .await
            .unwrap();
        let mut fixture = Self {
            pool: admin.clone().into(),
            raw: admin.clone(),
            url,
            schema,
            removed: false,
        };
        admin.close().await;
        let options = sqlx::postgres::PgConnectOptions::from_str(&fixture.url)
            .unwrap()
            .options([
                ("search_path", fixture.schema.as_str()),
                ("application_name", fixture.schema.as_str()),
            ]);
        fixture.raw = PgPoolOptions::new().connect_with(options).await.unwrap();
        fixture.pool = fixture.raw.clone().into();
        db::run_migrations(&fixture.pool)
            .await
            .expect("native PostgreSQL migration");
        fixture
    }

    /// Drops the schema. Call it at the end of the test, after its servers and tasks have
    /// stopped; Drop does the same if the test never gets there.
    pub async fn finish(&mut self) {
        if self.removed {
            return;
        }
        if let Err(error) = drop_schema(&self.raw, &self.url, &self.schema).await {
            panic!("dropping test schema {} failed: {error}", self.schema);
        }
        self.removed = true;
    }
}

/// Ends the sessions the test left open (a killed server's last queries, a pool held by a
/// spawned task), which would block the DROP, and then drops the schema.
async fn drop_schema(raw: &sqlx::PgPool, url: &str, schema: &str) -> Result<(), sqlx::Error> {
    let _ = tokio::time::timeout(Duration::from_secs(2), raw.close()).await;
    let options = sqlx::postgres::PgConnectOptions::from_str(url)?
        .options([("lock_timeout", "5s"), ("statement_timeout", "10s")]);
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await?;
    sqlx::query(
        "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
         WHERE application_name = $1 AND pid <> pg_backend_pid()",
    )
    .bind(schema)
    .execute(&admin)
    .await?;
    let mut attempt = 1;
    loop {
        match sqlx::query(&format!("DROP SCHEMA \"{schema}\" CASCADE"))
            .execute(&admin)
            .await
        {
            Ok(_) => break,
            Err(error) if attempt < 5 => {
                eprintln!("dropping {schema} failed (attempt {attempt}): {error}");
                attempt += 1;
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            Err(error) => return Err(error),
        }
    }
    admin.close().await;
    Ok(())
}

impl Drop for PostgresDatabase {
    fn drop(&mut self) {
        if self.removed {
            return;
        }
        let raw = self.raw.clone();
        let url = self.url.clone();
        let schema = self.schema.clone();
        let result = std::thread::spawn(move || {
            tokio::runtime::Runtime::new()
                .unwrap()
                .block_on(async move { drop_schema(&raw, &url, &schema).await })
        })
        .join();
        let failure = match result {
            Ok(Ok(())) => return,
            Ok(Err(error)) => error.to_string(),
            Err(_) => "cleanup thread panicked".to_owned(),
        };
        if std::thread::panicking() {
            eprintln!("test schema cleanup failed: {failure}");
        } else {
            panic!("test schema cleanup failed: {failure}");
        }
    }
}

pub fn token() -> String {
    format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}
