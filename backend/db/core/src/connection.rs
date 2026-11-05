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
    Postgres(crate::pool::PgPool),
    Sqlite(crate::pool::SqlitePool),
}

impl From<sqlx::PgPool> for DbPool {
    fn from(pool: sqlx::PgPool) -> Self {
        Self::Postgres(pool.into())
    }
}

impl From<sqlx::SqlitePool> for DbPool {
    fn from(pool: sqlx::SqlitePool) -> Self {
        Self::Sqlite(pool.into())
    }
}

impl From<crate::pool::PgPool> for DbPool {
    fn from(pool: crate::pool::PgPool) -> Self {
        Self::Postgres(pool)
    }
}
impl From<crate::pool::SqlitePool> for DbPool {
    fn from(pool: crate::pool::SqlitePool) -> Self {
        Self::Sqlite(pool)
    }
}

// SQLx warns with both the name and value of unknown PostgreSQL options. Validate before
// calling its parser so a misspelled password option cannot reach the logger.
pub(crate) fn postgres_options(database_url: &str) -> Result<PgConnectOptions> {
    let url = url::Url::parse(database_url)?;
    for (name, _) in url.query_pairs() {
        let supported = matches!(
            name.as_ref(),
            "sslmode"
                | "ssl-mode"
                | "sslrootcert"
                | "ssl-root-cert"
                | "ssl-ca"
                | "sslcert"
                | "ssl-cert"
                | "sslkey"
                | "ssl-key"
                | "statement-cache-capacity"
                | "host"
                | "hostaddr"
                | "port"
                | "dbname"
                | "user"
                | "password"
                | "application_name"
                | "options"
        ) || (name.starts_with("options[") && name.ends_with(']'));
        anyhow::ensure!(supported, "Unsupported PostgreSQL connection parameter");
    }
    // Day boundaries in reports are UTC whatever the server's time zone is.
    Ok(PgConnectOptions::from_str(database_url)?.options([("timezone", "UTC")]))
}

fn sqlite_memory(database_url: &str) -> bool {
    let sqlite_url = database_url
        .strip_prefix("sqlite://")
        .or_else(|| database_url.strip_prefix("sqlite:"))
        .unwrap_or(database_url);
    let (database, query) = sqlite_url.split_once('?').unwrap_or((sqlite_url, ""));
    database == ":memory:"
        || url::form_urlencoded::parse(query.as_bytes())
            .any(|(name, value)| name == "mode" && value == "memory")
}

/// The database file a `sqlite:` URL names, or `None` for PostgreSQL and in-memory SQLite.
pub fn sqlite_file(database_url: &str) -> Result<Option<std::path::PathBuf>> {
    if !database_url.starts_with("sqlite:") || sqlite_memory(database_url) {
        return Ok(None);
    }
    Ok(Some(
        SqliteConnectOptions::from_str(database_url)?
            .get_filename()
            .to_owned(),
    ))
}

#[cfg(unix)]
fn sqlite_file_path(
    database_url: &str,
    options: &SqliteConnectOptions,
) -> Option<std::path::PathBuf> {
    (!sqlite_memory(database_url)).then(|| options.get_filename().to_owned())
}

#[cfg(unix)]
fn secure_sqlite_files(path: &Path, create_main: bool) -> std::io::Result<()> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    match std::fs::metadata(path) {
        Ok(metadata) if metadata.is_file() => {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?
        }
        Ok(_) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "SQLite target is not a regular file",
            ))
        }
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
        match std::fs::metadata(&sidecar) {
            Ok(metadata) if metadata.is_file() => {
                std::fs::set_permissions(&sidecar, std::fs::Permissions::from_mode(0o600))?
            }
            Ok(_) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "SQLite sidecar is not a regular file",
                ))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

impl DbPool {
    pub(crate) fn set_cell_identity(
        &self,
        cell: codeyard_support::cells::Prefix,
        prefix: codeyard_support::cells::Prefix,
    ) {
        match self {
            Self::Postgres(pool) => pool.set_cell_identity(cell, prefix),
            Self::Sqlite(pool) => pool.set_cell_identity(cell, prefix),
        }
    }
    pub fn cell_id(&self) -> codeyard_support::cells::Prefix {
        match self {
            Self::Postgres(pool) => pool.cell_id(),
            Self::Sqlite(pool) => pool.cell_id(),
        }
    }
    pub fn cell_prefix(&self) -> codeyard_support::cells::Prefix {
        match self {
            Self::Postgres(pool) => pool.cell_prefix(),
            Self::Sqlite(pool) => pool.cell_prefix(),
        }
    }
    pub fn set_log_objects(&self, store: std::sync::Arc<dyn crate::ci_logs::LogObjects>) {
        match self {
            Self::Postgres(pool) => pool.set_log_objects(store),
            Self::Sqlite(pool) => pool.set_log_objects(store),
        }
    }
    pub fn log_objects(&self) -> Result<std::sync::Arc<dyn crate::ci_logs::LogObjects>> {
        match self {
            Self::Postgres(pool) => pool.log_objects(),
            Self::Sqlite(pool) => pool.log_objects(),
        }
    }
    /// Whether the database answers a trivial query.
    pub async fn ping(&self) -> Result<()> {
        match self {
            Self::Postgres(pool) => sqlx::query("SELECT 1").execute(pool).await.map(drop)?,
            Self::Sqlite(pool) => sqlx::query("SELECT 1").execute(pool).await.map(drop)?,
        }
        Ok(())
    }

    /// Authentication uses the database clock across different web nodes.
    pub async fn unix_time(&self) -> Result<i64> {
        tokio::time::timeout(Duration::from_secs(2), async {
            match self {
                Self::Postgres(pool) => {
                    sqlx::query_scalar(
                        "SELECT FLOOR(EXTRACT(EPOCH FROM clock_timestamp()))::bigint",
                    )
                    .fetch_one(pool)
                    .await
                }
                Self::Sqlite(pool) => {
                    sqlx::query_scalar("SELECT CAST(strftime('%s','now') AS INTEGER)")
                        .fetch_one(pool)
                        .await
                }
            }
        })
        .await?
        .map_err(Into::into)
    }

    /// Open connections, including idle connections, for this process.
    pub fn size(&self) -> u32 {
        match self {
            Self::Postgres(pool) => {
                let replica = pool
                    .read_replica
                    .read()
                    .unwrap_or_else(|error| error.into_inner());
                pool.size() + replica.as_ref().map_or(0, |replica| replica.size())
            }
            Self::Sqlite(pool) => pool.size(),
        }
    }

    /// Requests waiting to obtain a connection from this process's application pool.
    pub fn waiters(&self) -> usize {
        match self {
            Self::Postgres(pool) => {
                let replica = pool
                    .read_replica
                    .read()
                    .unwrap_or_else(|error| error.into_inner());
                pool.waiters() + replica.as_ref().map_or(0, |replica| replica.waiters())
            }
            Self::Sqlite(pool) => pool.waiters(),
        }
    }

    /// Allows owned database listeners to stop retrying after pool shutdown.
    pub fn is_closed(&self) -> bool {
        match self {
            Self::Postgres(pool) => pool.is_closed(),
            Self::Sqlite(pool) => pool.is_closed(),
        }
    }

    pub async fn close(&self) {
        // SQLx waits for checked-out connections. A stuck request must not prevent the
        // process from reaching its runtime shutdown deadline.
        if tokio::time::timeout(Duration::from_secs(10), async {
            match self {
                Self::Postgres(pool) => {
                    let replica = pool
                        .read_replica
                        .read()
                        .unwrap_or_else(|error| error.into_inner())
                        .clone();
                    if let Some(replica) = replica {
                        tokio::join!(pool.close(), replica.close());
                    } else {
                        pool.close().await;
                    }
                }
                Self::Sqlite(pool) => pool.close().await,
            }
        })
        .await
        .is_err()
        {
            tracing::warn!("database connections did not return during shutdown");
        }
    }
}

#[cfg(test)]
mod shutdown_tests {
    use super::*;

    #[tokio::test]
    async fn pool_close_has_a_deadline_even_when_a_connection_is_checked_out() {
        let directory = tempfile::tempdir().unwrap();
        let pool = init_db(&format!(
            "sqlite://{}",
            directory.path().join("test.db").display()
        ))
        .await
        .unwrap();
        let DbPool::Sqlite(raw) = &pool else {
            unreachable!()
        };
        let held = raw.acquire().await.unwrap();
        tokio::time::pause();
        let start = tokio::time::Instant::now();
        pool.close().await;
        assert!(pool.is_closed());
        assert!(
            (Duration::from_secs(10)..=Duration::from_millis(10_002)).contains(&start.elapsed())
        );
        drop(held);
    }
    #[tokio::test]
    async fn both_postgres_pools_close_when_the_replica_has_a_stuck_reader() {
        let url = std::env::var("GITSERV_TEST_DATABASE_URL")
            .expect("builder-test.sh supplies PostgreSQL");
        let primary: crate::pool::PgPool = PgPoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await
            .unwrap()
            .into();
        let replica: crate::pool::PgPool = PgPoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await
            .unwrap()
            .into();
        *primary.read_replica.write().unwrap() = Some(replica.clone());
        let pool = DbPool::Postgres(primary.clone());
        let held = replica.acquire().await.unwrap();
        assert_eq!(pool.size(), primary.size() + replica.size());
        tokio::time::pause();
        pool.close().await;
        assert!(primary.is_closed());
        assert!(replica.is_closed());
        drop(held);
    }
}

pub(crate) fn postgres_pool_size(raw: Option<&str>) -> Result<u32> {
    let size = raw
        .unwrap_or("20")
        .parse::<u32>()
        .map_err(|_| anyhow::anyhow!("DATABASE_POOL_SIZE must be between 1 and 1000"))?;
    anyhow::ensure!(
        (1..=1000).contains(&size),
        "DATABASE_POOL_SIZE must be between 1 and 1000"
    );
    Ok(size)
}

pub async fn init_db(database_url: &str) -> Result<DbPool> {
    // Never trace credential-bearing URLs or connection options.
    if database_url.starts_with("postgres://") || database_url.starts_with("postgresql://") {
        info!("Connecting to PostgreSQL");
        let options = postgres_options(database_url)?;
        let configured_pool = match std::env::var("DATABASE_POOL_SIZE") {
            Ok(raw) => Some(raw),
            Err(std::env::VarError::NotPresent) => None,
            Err(_) => bail!("DATABASE_POOL_SIZE must be between 1 and 1000"),
        };
        // A request that cannot get a connection in 5 seconds fails instead of hanging
        // for the 30-second default while the database is down.
        return Ok(PgPoolOptions::new()
            .max_connections(postgres_pool_size(configured_pool.as_deref())?)
            .test_before_acquire(true)
            .acquire_timeout(Duration::from_secs(5))
            .connect_with(options)
            .await?
            .into());
    }
    if database_url.starts_with("sqlite:") {
        info!("Connecting to SQLite");
        let options = SqliteConnectOptions::from_str(database_url)?
            .with_regexp()
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
        let pool_options = if sqlite_memory(database_url) {
            SqlitePoolOptions::new()
                .max_connections(1)
                .min_connections(1)
                .idle_timeout(None)
                .max_lifetime(None)
        } else {
            SqlitePoolOptions::new().max_connections(8)
        };
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
        let options = postgres_options(database_url)?;
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

    #[tokio::test]
    async fn postgres_connections_start_tls_when_the_server_supports_it() {
        use sqlx::Connection;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        for mode in ["require", "prefer", "verify-full"] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let peer = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = [0; 8];
                socket.read_exact(&mut request).await.unwrap();
                assert_eq!(request, [0, 0, 0, 8, 4, 210, 22, 47]);
                socket.write_all(b"S").await.unwrap();
                let mut record = [0; 5];
                socket.read_exact(&mut record).await.unwrap();
                assert_eq!(record[0], 22, "TLS handshake record");
                assert_eq!(record[1], 3);
            });
            let url = format!("postgres://fixture:fixture@{address}/fixture?sslmode={mode}");
            assert!(tokio::time::timeout(
                Duration::from_secs(3),
                sqlx::PgConnection::connect_with(&postgres_options(&url).unwrap())
            )
            .await
            .unwrap()
            .is_err());
            peer.await.unwrap();
        }
    }

    #[test]
    fn postgres_pool_size_is_bounded_without_echoing_input() {
        assert_eq!(postgres_pool_size(None).unwrap(), 20);
        assert_eq!(postgres_pool_size(Some("1")).unwrap(), 1);
        assert_eq!(postgres_pool_size(Some("1000")).unwrap(), 1000);
        for raw in ["0", "1001", "-1", "private-value", ""] {
            assert_eq!(
                postgres_pool_size(Some(raw)).unwrap_err().to_string(),
                "DATABASE_POOL_SIZE must be between 1 and 1000"
            );
        }
    }

    #[tokio::test]
    async fn unknown_postgres_options_are_rejected_without_logging_values() {
        let output = Arc::new(Mutex::new(Vec::new()));
        let writer = Capture(output.clone());
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        let _logging = tracing::subscriber::set_default(subscriber);
        for name in ["sslpassword", "Password", "pass%77ord_typo"] {
            let url = format!("postgres://fixture@localhost/unused?{name}=private-key-password");
            for result in [init_db(&url).await, open_source(&url).await] {
                let error = result.err().expect("reject before connecting").to_string();
                assert_eq!(error, "Unsupported PostgreSQL connection parameter");
            }
        }
        let logged = String::from_utf8(output.lock().unwrap().clone()).unwrap();
        assert!(!logged.contains("private-key-password"));
        assert!(!logged.contains("unrecognized connect parameter"));
    }

    #[tokio::test]
    async fn named_private_memory_database_uses_one_connection() {
        let pool = init_db("sqlite://scratch?mode=memory&cache=private")
            .await
            .unwrap();
        let DbPool::Sqlite(raw) = &pool else {
            unreachable!()
        };
        assert_eq!(raw.options().get_max_connections(), 1);
        crate::run_migrations(&pool).await.unwrap();
        let (a, b) = tokio::join!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM users").fetch_one(raw),
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM users").fetch_one(raw),
        );
        assert_eq!(a.unwrap(), b.unwrap());
        pool.close().await;
    }

    #[tokio::test]
    async fn memory_pools_do_not_recycle_the_database() {
        for url in [
            "sqlite::memory:",
            "sqlite://scratch?mode=memory&cache=private",
        ] {
            let pool = init_db(url).await.unwrap();
            let DbPool::Sqlite(raw) = &pool else {
                unreachable!()
            };
            assert_eq!(raw.options().get_min_connections(), 1);
            assert_eq!(raw.options().get_idle_timeout(), None);
            assert_eq!(raw.options().get_max_lifetime(), None);
            pool.close().await;
        }
    }

    #[tokio::test]
    async fn connection_diagnostic_does_not_log_passwords_or_the_raw_url() {
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
        let _logging = tracing::subscriber::set_default(subscriber);
        let result = init_db(url).await;
        assert!(result.is_err());
        let logged = String::from_utf8(output.lock().unwrap().clone()).unwrap();
        assert!(logged.contains("Connecting to PostgreSQL"));
        assert!(!logged.contains(url));
        assert!(!logged.contains("logging-fixture-password"));
        assert!(!logged.contains("logging-fixture-query-password"));
    }
}

#[cfg(test)]
mod clock_tests {
    #[tokio::test]
    async fn both_database_clocks_return_current_unix_seconds() {
        let pg = std::env::var("GITSERV_TEST_DATABASE_URL")
            .expect("builder-test.sh supplies PostgreSQL");
        for url in ["sqlite::memory:", pg.as_str()] {
            let pool = super::init_db(url).await.unwrap();
            let before = chrono::Utc::now().timestamp();
            let actual = pool.unix_time().await.unwrap();
            let after = chrono::Utc::now().timestamp();
            assert!((before..=after).contains(&actual));
            pool.close().await;
        }
    }
}
