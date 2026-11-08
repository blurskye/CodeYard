//! Opening the application database (SQLite or PostgreSQL).

use anyhow::{bail, Context, Result};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
#[cfg(unix)]
use std::path::Path;
use std::{str::FromStr, time::Duration};
use tracing::{info, warn};

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
        cell: codeyard_types::cell_ids::Prefix,
        prefix: codeyard_types::cell_ids::Prefix,
        prefixes: Vec<codeyard_types::cell_ids::Prefix>,
    ) {
        match self {
            Self::Postgres(pool) => pool.set_cell_identity(cell, prefix, prefixes),
            Self::Sqlite(pool) => pool.set_cell_identity(cell, prefix, prefixes),
        }
    }
    pub fn owns_prefix(&self, prefix: codeyard_types::cell_ids::Prefix) -> bool {
        match self {
            Self::Postgres(pool) => pool.owns_prefix(prefix),
            Self::Sqlite(pool) => pool.owns_prefix(prefix),
        }
    }
    pub fn cell_id(&self) -> codeyard_types::cell_ids::Prefix {
        match self {
            Self::Postgres(pool) => pool.cell_id(),
            Self::Sqlite(pool) => pool.cell_id(),
        }
    }
    pub fn cell_prefix(&self) -> codeyard_types::cell_ids::Prefix {
        match self {
            Self::Postgres(pool) => pool.cell_prefix(),
            Self::Sqlite(pool) => pool.cell_prefix(),
        }
    }
    pub fn set_directory(&self, provider: crate::cells::directory::Provider) -> Result<()> {
        match self {
            Self::Postgres(pool) => pool.set_directory(provider),
            Self::Sqlite(_) => {
                bail!("Copy this SQLite cell to PostgreSQL before enrolling it in the directory")
            }
        }
        Ok(())
    }
    pub fn directory(&self) -> Option<crate::cells::directory::Provider> {
        match self {
            Self::Postgres(pool) => pool.directory(),
            Self::Sqlite(pool) => pool.directory(),
        }
    }
    pub fn set_search(&self, provider: crate::cells::search::Provider) -> Result<()> {
        match self {
            Self::Postgres(pool) => pool.set_search(provider),
            Self::Sqlite(_) => {
                bail!("Copy this SQLite cell to PostgreSQL before enrolling it in the directory")
            }
        }
        Ok(())
    }
    pub fn search(&self) -> Option<crate::cells::search::Provider> {
        match self {
            Self::Postgres(pool) => pool.search(),
            Self::Sqlite(pool) => pool.search(),
        }
    }
    pub fn set_foreign_authorization(&self, provider: crate::cells::authorization::Provider) {
        match self {
            Self::Postgres(pool) => pool.set_foreign_authorization(provider),
            Self::Sqlite(pool) => pool.set_foreign_authorization(provider),
        }
    }
    pub fn foreign_authorization(&self) -> Option<crate::cells::authorization::Provider> {
        match self {
            Self::Postgres(pool) => pool.foreign_authorization(),
            Self::Sqlite(pool) => pool.foreign_authorization(),
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

fn is_postgres(database_url: &str) -> bool {
    database_url.starts_with("postgres://") || database_url.starts_with("postgresql://")
}

/// How long startup keeps retrying an unavailable PostgreSQL server:
/// `DATABASE_CONNECT_WAIT_SECONDS`, 0 to 3,600, default 60.
pub(crate) fn connect_wait(raw: Option<&str>) -> Result<Duration> {
    raw.map_or(Some(60), |raw| {
        raw.parse::<u64>().ok().filter(|seconds| *seconds <= 3600)
    })
    .map(Duration::from_secs)
    .context("DATABASE_CONNECT_WAIT_SECONDS must be between 0 and 3600")
}

/// Why opening or migrating PostgreSQL failed in a way that can clear by itself: the server
/// is unreachable, starting, stopping, still a standby or busy. Built from error kinds and
/// SQLSTATE codes only, so no server text or connection setting reaches the log.
fn retry_reason(error: &anyhow::Error) -> Option<String> {
    error.chain().find_map(|cause| {
        if cause.is::<tokio::time::error::Elapsed>() {
            return Some("timed out".to_owned());
        }
        match cause.downcast_ref::<sqlx::Error>()? {
            sqlx::Error::Io(error) => Some(format!("connection failed ({:?})", error.kind())),
            sqlx::Error::Tls(_) => Some("TLS handshake failed".to_owned()),
            sqlx::Error::PoolTimedOut => Some("timed out".to_owned()),
            // 08: connection exception; 57P: shutting down or starting up; 25006: a standby
            // not yet promoted; 53300: too many connections; 55P03: another node is migrating.
            sqlx::Error::Database(error) => error
                .code()
                .filter(|code| {
                    code.starts_with("08")
                        || code.starts_with("57P")
                        || matches!(code.as_ref(), "25006" | "53300" | "55P03")
                })
                .map(|code| format!("SQLSTATE {code}")),
            _ => None,
        }
    })
}

const FIRST_RETRY: Duration = Duration::from_millis(500);
const LONGEST_RETRY: Duration = Duration::from_secs(10);

/// Opens and migrates the application database for a command or server start. PostgreSQL
/// can be unreachable for a while when a node starts, during a failover or before the
/// database itself is up, so failures that can clear by themselves are retried with doubling
/// delays for up to `DATABASE_CONNECT_WAIT_SECONDS`. SQLite is opened once.
pub async fn open_migrated(database_url: &str) -> Result<DbPool> {
    let raw = match std::env::var("DATABASE_CONNECT_WAIT_SECONDS") {
        Ok(raw) => Some(raw),
        Err(std::env::VarError::NotPresent) => None,
        Err(_) => bail!("DATABASE_CONNECT_WAIT_SECONDS must be between 0 and 3600"),
    };
    let wait = connect_wait(raw.as_deref())?;
    let wait = if is_postgres(database_url) {
        wait
    } else {
        Duration::ZERO
    };
    open_migrated_within(database_url, wait).await
}

async fn open_migrated_within(database_url: &str, wait: Duration) -> Result<DbPool> {
    let deadline = tokio::time::Instant::now() + wait;
    let mut delay = FIRST_RETRY;
    loop {
        let (error, opened) = match init_db(database_url).await {
            Err(error) => (error, false),
            Ok(pool) => match crate::run_migrations(&pool).await {
                Ok(()) => return Ok(pool),
                Err(error) => {
                    pool.close().await;
                    (error, true)
                }
            },
        };
        let reason = retry_reason(&error);
        match reason {
            Some(reason) if tokio::time::Instant::now() + delay <= deadline => {
                warn!(
                    %reason,
                    retry_in_ms = u64::try_from(delay.as_millis()).unwrap_or(u64::MAX),
                    "the database is not available yet; retrying"
                );
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(LONGEST_RETRY);
            }
            _ if opened => return Err(error),
            // The driver's text can repeat connection settings, so only the reason is shown.
            Some(reason) => bail!("could not open the configured application database: {reason}"),
            None => bail!("could not open the configured application database"),
        }
    }
}

pub async fn init_db(database_url: &str) -> Result<DbPool> {
    // Never trace credential-bearing URLs or connection options.
    if is_postgres(database_url) {
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

/// One connection for session-level work: advisory leases that last as long as the
/// connection, and migrations. It comes from `DATABASE_DIRECT_URL` when that is set, so a
/// transaction-pooling PgBouncer in front of `DATABASE_URL` never hands a lease's server
/// session to another client; otherwise it is taken out of `pool`. The direct URL must reach
/// the same database and schema, which is checked, because a lease taken elsewhere would
/// exclude nothing.
pub async fn session_connection(pool: &crate::pool::PgPool) -> Result<sqlx::PgConnection> {
    use sqlx::ConnectOptions;
    let url = match std::env::var("DATABASE_DIRECT_URL") {
        Ok(url) if !url.is_empty() => url,
        Ok(_) | Err(std::env::VarError::NotPresent) => {
            let mut connection = pool.acquire().await?;
            connection.close_on_drop();
            return Ok(connection.detach());
        }
        Err(_) => bail!("invalid DATABASE_DIRECT_URL"),
    };
    if !is_postgres(&url) {
        bail!("DATABASE_DIRECT_URL requires PostgreSQL");
    }
    let options = postgres_options(&url)?;
    let mut direct = tokio::time::timeout(Duration::from_secs(5), options.connect())
        .await
        .context("DATABASE_DIRECT_URL did not answer within 5 seconds")??;
    const PLACE: &str = "SELECT current_database()::text,COALESCE(current_schema(),'')::text";
    let here: (String, String) = sqlx::query_as(PLACE).fetch_one(&mut direct).await?;
    let there: (String, String) = sqlx::query_as(PLACE).fetch_one(pool).await?;
    if here != there {
        bail!("DATABASE_DIRECT_URL reaches another database or schema than DATABASE_URL");
    }
    Ok(direct)
}

/// Opens an existing database for reading. Unlike `init_db`, it never creates a SQLite
/// file and leaves the file's journal mode and permissions alone.
pub async fn open_source(database_url: &str) -> Result<DbPool> {
    if is_postgres(database_url) {
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
    pub(super) struct Capture(pub(super) Arc<Mutex<Vec<u8>>>);

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
mod startup_tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// Runs `body` with warnings written to a buffer, then returns all of them and the
    /// retry warnings among them.
    async fn warnings<T>(body: impl std::future::Future<Output = T>) -> (T, String, Vec<String>) {
        let output = Arc::new(Mutex::new(Vec::new()));
        let writer = super::safe_logging_tests::Capture(output.clone());
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_max_level(tracing::Level::WARN)
            .with_writer(move || writer.clone())
            .finish();
        let _logging = tracing::subscriber::set_default(subscriber);
        let result = body.await;
        let logged = String::from_utf8(output.lock().unwrap().clone()).unwrap();
        let retries = logged
            .lines()
            .filter(|line| line.contains("WARN") && line.contains("retrying"))
            .map(str::to_owned)
            .collect();
        (result, logged, retries)
    }

    fn closed_port() -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    }

    #[test]
    fn connect_wait_is_bounded_without_echoing_input() {
        assert_eq!(connect_wait(None).unwrap(), Duration::from_secs(60));
        assert_eq!(connect_wait(Some("0")).unwrap(), Duration::ZERO);
        assert_eq!(
            connect_wait(Some("3600")).unwrap(),
            Duration::from_secs(3600)
        );
        for raw in ["3601", "-1", "private-value", ""] {
            assert_eq!(
                connect_wait(Some(raw)).unwrap_err().to_string(),
                "DATABASE_CONNECT_WAIT_SECONDS must be between 0 and 3600"
            );
        }
    }

    #[tokio::test]
    async fn an_unreachable_server_is_retried_until_the_wait_runs_out() {
        tokio::time::pause();
        let url = format!(
            "postgres://fixture:retry-fixture-password@127.0.0.1:{}/unused",
            closed_port()
        );
        let start = tokio::time::Instant::now();
        let wait = Duration::from_secs(30);
        let (result, logged, retries) = warnings(open_migrated_within(&url, wait)).await;
        let error = result.err().expect("nothing listens").to_string();
        assert!(
            error.starts_with("could not open the configured application database: "),
            "{error}"
        );
        // Each attempt is bounded by the five-second acquisition deadline, and no retry
        // starts after the wait.
        let elapsed = start.elapsed();
        assert!(
            (wait - LONGEST_RETRY..=wait + Duration::from_secs(6)).contains(&elapsed),
            "{elapsed:?}"
        );
        assert!(retries.len() >= 3, "{logged}");
        assert!(!logged.contains("retry-fixture-password"));
    }

    #[tokio::test]
    async fn sqlite_and_settings_errors_are_not_retried() {
        tokio::time::pause();
        let start = tokio::time::Instant::now();
        let directory = tempfile::tempdir().unwrap();
        let missing = directory.path().join("missing").join("app.db");
        let sqlite = format!("sqlite://{}", missing.display());
        let (result, _, retries) = warnings(open_migrated(&sqlite)).await;
        assert!(result.is_err());
        let typo = "postgres://fixture@127.0.0.1:1/unused?sslpassword=private";
        let (typo_result, _, typo_retries) = warnings(open_migrated(typo)).await;
        assert_eq!(
            typo_result.err().unwrap().to_string(),
            "could not open the configured application database"
        );
        assert!(retries.is_empty() && typo_retries.is_empty());
        assert_eq!(start.elapsed(), Duration::ZERO);
    }

    /// PostgreSQL behind a TCP relay that drops its first `drops` connections, as a pooler
    /// does while the primary is being replaced.
    async fn flaky_relay(drops: usize) -> (String, tokio::task::JoinHandle<()>) {
        let mut url = url::Url::parse(
            &std::env::var("GITSERV_TEST_DATABASE_URL")
                .expect("builder-test.sh supplies PostgreSQL"),
        )
        .unwrap();
        let server = format!("{}:{}", url.host_str().unwrap(), url.port().unwrap_or(5432));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        url.set_port(Some(listener.local_addr().unwrap().port()))
            .unwrap();
        let relay = tokio::spawn(async move {
            let mut accepted = 0;
            loop {
                let (mut client, _) = listener.accept().await.unwrap();
                accepted += 1;
                if accepted <= drops {
                    drop(client);
                    continue;
                }
                let server = server.clone();
                tokio::spawn(async move {
                    let mut upstream = tokio::net::TcpStream::connect(server).await.unwrap();
                    let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
                });
            }
        });
        (url.to_string(), relay)
    }

    #[tokio::test]
    async fn a_server_that_drops_the_first_connections_is_waited_for() {
        let (url, relay) = flaky_relay(2).await;
        let (result, logged, retries) =
            warnings(open_migrated_within(&url, Duration::from_secs(30))).await;
        let pool = result.unwrap();
        pool.ping().await.unwrap();
        pool.close().await;
        relay.abort();
        assert_eq!(retries.len(), 2, "{logged}");
    }

    #[tokio::test]
    async fn a_refused_password_is_not_retried() {
        let (url, relay) = flaky_relay(0).await;
        let mut url = url::Url::parse(&url).unwrap();
        url.set_password(Some("not-the-password")).unwrap();
        let started = std::time::Instant::now();
        let (result, logged, retries) =
            warnings(open_migrated_within(url.as_str(), Duration::from_secs(30))).await;
        relay.abort();
        assert_eq!(
            result.err().unwrap().to_string(),
            "could not open the configured application database"
        );
        assert!(retries.is_empty(), "{logged}");
        assert!(started.elapsed() < Duration::from_secs(10));
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
