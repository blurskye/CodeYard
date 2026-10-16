//! Opening the application database (SQLite or PostgreSQL).

use anyhow::{bail, Result};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
#[cfg(unix)]
use std::path::Path;
use std::{str::FromStr, time::Duration};
use tracing::info;

/// Connection pool for whichever database `DATABASE_URL` selects.
#[derive(Clone)]
pub enum DbPool {
    Postgres(sqlx::PgPool),
    Sqlite(sqlx::SqlitePool),
}

impl From<sqlx::PgPool> for DbPool {
    fn from(pool: sqlx::PgPool) -> Self {
        Self::Postgres(pool)
    }
}

impl From<sqlx::SqlitePool> for DbPool {
    fn from(pool: sqlx::SqlitePool) -> Self {
        Self::Sqlite(pool)
    }
}

#[cfg(unix)]
fn sqlite_file_path(
    database_url: &str,
    options: &SqliteConnectOptions,
) -> Option<std::path::PathBuf> {
    let sqlite_url = database_url
        .strip_prefix("sqlite://")
        .or_else(|| database_url.strip_prefix("sqlite:"))
        .unwrap_or(database_url);
    let (database, query) = sqlite_url.split_once('?').unwrap_or((sqlite_url, ""));
    let memory_mode = database == ":memory:"
        || url::form_urlencoded::parse(query.as_bytes())
            .any(|(name, value)| name == "mode" && value == "memory");
    (!memory_mode).then(|| options.get_filename().to_owned())
}

#[cfg(unix)]
fn secure_sqlite_files(path: &Path, create_main: bool) -> std::io::Result<()> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    match std::fs::metadata(path) {
        Ok(_) => std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && create_main => {
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(path)?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    for suffix in ["-wal", "-shm"] {
        let mut name = path.as_os_str().to_owned();
        name.push(suffix);
        let sidecar = std::path::PathBuf::from(name);
        match std::fs::set_permissions(&sidecar, std::fs::Permissions::from_mode(0o600)) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

impl DbPool {
    /// Whether the database answers a trivial query.
    pub async fn ping(&self) -> Result<()> {
        match self {
            Self::Postgres(pool) => sqlx::query("SELECT 1").execute(pool).await.map(drop)?,
            Self::Sqlite(pool) => sqlx::query("SELECT 1").execute(pool).await.map(drop)?,
        }
        Ok(())
    }

    /// Allows owned database listeners to stop retrying after pool shutdown.
    pub fn is_closed(&self) -> bool {
        match self {
            Self::Postgres(pool) => pool.is_closed(),
            Self::Sqlite(pool) => pool.is_closed(),
        }
    }

    pub async fn close(&self) {
        match self {
            Self::Postgres(pool) => pool.close().await,
            Self::Sqlite(pool) => pool.close().await,
        }
    }
}

pub async fn init_db(database_url: &str) -> Result<DbPool> {
    // Never trace credential-bearing URLs or connection options.
    if database_url.starts_with("postgres://") || database_url.starts_with("postgresql://") {
        info!("Connecting to PostgreSQL");
        let options = PgConnectOptions::from_str(database_url)?;
        // A request that cannot get a connection in 5 seconds fails instead of hanging
        // for the 30-second default while the database is down.
        return Ok(PgPoolOptions::new()
            .max_connections(20)
            .acquire_timeout(Duration::from_secs(5))
            .connect_with(options)
            .await?
            .into());
    }
    if database_url.starts_with("sqlite:") {
        info!("Connecting to SQLite");
        let options = SqliteConnectOptions::from_str(database_url)?
            .create_if_missing(true)
            .foreign_keys(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Full)
            .busy_timeout(Duration::from_secs(3));
        #[cfg(unix)]
        let sqlite_path = sqlite_file_path(database_url, &options);
        #[cfg(unix)]
        if let Some(path) = &sqlite_path {
            // SQLite copies the main database's mode when it creates journals. Create
            // or repair that file before SQLite can make a WAL or shared-memory file.
            secure_sqlite_files(path, true)?;
        }
        // Each in-memory connection is its own database, so keep just one.
        let maximum = if database_url.contains(":memory:") {
            1
        } else {
            8
        };
        let pool_options = SqlitePoolOptions::new().max_connections(maximum);
        #[cfg(unix)]
        let pool_options = if let Some(path) = sqlite_path.clone() {
            pool_options.after_connect(move |_, _| {
                let path = path.clone();
                Box::pin(async move { secure_sqlite_files(&path, false).map_err(sqlx::Error::Io) })
            })
        } else {
            pool_options
        };
        let pool = pool_options.connect_with(options).await?;
        #[cfg(unix)]
        if let Some(path) = &sqlite_path {
            secure_sqlite_files(path, false)?;
        }
        return Ok(pool.into());
    }
    bail!("Unsupported application database scheme")
}

/// Opens an existing database for reading. Unlike `init_db`, it never creates a SQLite
/// file and leaves the file's journal mode and permissions alone.
pub async fn open_source(database_url: &str) -> Result<DbPool> {
    if database_url.starts_with("postgres://") || database_url.starts_with("postgresql://") {
        let options = PgConnectOptions::from_str(database_url)?;
        return Ok(PgPoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await?
            .into());
    }
    if database_url.starts_with("sqlite:") {
        let options = SqliteConnectOptions::from_str(database_url)?
            .read_only(true)
            .create_if_missing(false);
        return Ok(SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await?
            .into());
    }
    bail!("Unsupported application database scheme")
}

#[cfg(test)]
mod safe_logging_tests {
    use super::*;
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    #[derive(Clone)]
    struct Capture(Arc<Mutex<Vec<u8>>>);

    impl Write for Capture {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn connection_diagnostic_does_not_log_passwords_or_the_raw_url() {
        let output = Arc::new(Mutex::new(Vec::new()));
        let writer = Capture(output.clone());
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        // An invalid IPv6 literal fails URL parsing before options/pgpass,
        // connection establishment, or any service access.
        let url = "postgres://fixture:logging-fixture-password@[not-ipv6]/unused?password=logging-fixture-query-password";
        let result = tracing::subscriber::with_default(subscriber, || {
            futures::executor::block_on(init_db(url))
        });
        assert!(result.is_err());
        let logged = String::from_utf8(output.lock().unwrap().clone()).unwrap();
        assert!(logged.contains("Connecting to PostgreSQL"));
        assert!(!logged.contains(url));
        assert!(!logged.contains("logging-fixture-password"));
        assert!(!logged.contains("logging-fixture-query-password"));
    }
}
