//! SQLx connections with acquisition waiters shared by every handle of an application pool.
use futures_util::{future::BoxFuture, stream::BoxStream, StreamExt};
use sqlx::{
    pool::{MaybePoolConnection, PoolConnection},
    Database, Describe, Either, Error, Execute, Executor, Transaction,
};
use std::{
    borrow::Cow,
    future::{poll_fn, Future},
    ops::Deref,
    pin::pin,
    sync::{
        atomic::{AtomicU32, AtomicUsize, Ordering},
        Arc,
    },
};

pub type PgPool = Pool<sqlx::Postgres>;
pub type SqlitePool = Pool<sqlx::Sqlite>;

pub struct Pool<DB: Database> {
    inner: sqlx::Pool<DB>,
    waiting: Arc<AtomicUsize>,
    cell_identity: Arc<AtomicU32>,
    cell_prefixes: Arc<std::sync::RwLock<Vec<codeyard_types::cells::Prefix>>>,
    pub(crate) read_replica: Arc<std::sync::RwLock<Option<PgPool>>>,
    directory: Arc<std::sync::RwLock<Option<crate::cells::directory::Provider>>>,
    search: Arc<std::sync::RwLock<Option<crate::cells::search::Provider>>>,
    foreign_authorization: Arc<std::sync::RwLock<Option<crate::cells::authorization::Provider>>>,
    log_objects: Arc<std::sync::RwLock<Option<std::sync::Weak<dyn crate::ci_logs::LogObjects>>>>,
    request_counts: Arc<crate::cells::regions::Counts>,
}
impl<DB: Database> std::fmt::Debug for Pool<DB> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pool")
            .field("waiting", &self.waiters())
            .finish_non_exhaustive()
    }
}
impl<DB: Database> Clone for Pool<DB> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            waiting: self.waiting.clone(),
            cell_identity: self.cell_identity.clone(),
            cell_prefixes: self.cell_prefixes.clone(),
            read_replica: self.read_replica.clone(),
            log_objects: self.log_objects.clone(),
            foreign_authorization: self.foreign_authorization.clone(),
            directory: self.directory.clone(),
            search: self.search.clone(),
            request_counts: self.request_counts.clone(),
        }
    }
}
impl<DB: Database> From<sqlx::Pool<DB>> for Pool<DB> {
    fn from(inner: sqlx::Pool<DB>) -> Self {
        Self {
            inner,
            waiting: Arc::new(AtomicUsize::new(0)),
            cell_identity: Arc::default(),
            cell_prefixes: Arc::new(std::sync::RwLock::new(vec![
                codeyard_types::cells::Prefix::ZERO,
            ])),
            read_replica: Arc::default(),
            log_objects: Arc::default(),
            foreign_authorization: Arc::default(),
            directory: Arc::default(),
            search: Arc::default(),
            request_counts: Arc::default(),
        }
    }
}
impl<DB: Database> Deref for Pool<DB> {
    type Target = sqlx::Pool<DB>;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

struct Waiter {
    count: Arc<AtomicUsize>,
    waiting: bool,
}
impl Drop for Waiter {
    fn drop(&mut self) {
        if self.waiting {
            self.count.fetch_sub(1, Ordering::Relaxed);
        }
    }
}
impl<DB: Database> Pool<DB> {
    pub(crate) fn set_cell_identity(
        &self,
        cell: codeyard_types::cells::Prefix,
        prefix: codeyard_types::cells::Prefix,
        prefixes: Vec<codeyard_types::cells::Prefix>,
    ) {
        *self
            .cell_prefixes
            .write()
            .unwrap_or_else(|error| error.into_inner()) = prefixes;
        self.cell_identity.store(
            (u32::from(cell.get()) << 16) | u32::from(prefix.get()),
            Ordering::Release,
        );
    }
    pub fn owns_prefix(&self, prefix: codeyard_types::cells::Prefix) -> bool {
        self.cell_prefixes
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .binary_search(&prefix)
            .is_ok()
    }
    pub fn cell_id(&self) -> codeyard_types::cells::Prefix {
        codeyard_types::cells::Prefix::new(
            (self.cell_identity.load(Ordering::Acquire) >> 16) as u16,
        )
    }
    pub fn cell_prefix(&self) -> codeyard_types::cells::Prefix {
        codeyard_types::cells::Prefix::new(self.cell_identity.load(Ordering::Acquire) as u16)
    }
    pub(crate) fn same_pool(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.waiting, &other.waiting)
    }
    pub fn directory(&self) -> Option<crate::cells::directory::Provider> {
        self.directory
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }
    /// Requests routers forwarded, per account, region and day, waiting to be written.
    pub fn request_counts(&self) -> &crate::cells::regions::Counts {
        &self.request_counts
    }
    /// The global search index; only directory-enrolled PostgreSQL cells set it.
    pub fn search(&self) -> Option<crate::cells::search::Provider> {
        self.search
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }
    pub fn set_foreign_authorization(&self, provider: crate::cells::authorization::Provider) {
        *self
            .foreign_authorization
            .write()
            .unwrap_or_else(|error| error.into_inner()) = Some(provider);
    }
    pub fn foreign_authorization(&self) -> Option<crate::cells::authorization::Provider> {
        self.foreign_authorization
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }
    pub fn set_log_objects(&self, store: Arc<dyn crate::ci_logs::LogObjects>) {
        *self
            .log_objects
            .write()
            .unwrap_or_else(|error| error.into_inner()) = Some(Arc::downgrade(&store));
    }
    pub fn log_objects(&self) -> anyhow::Result<Arc<dyn crate::ci_logs::LogObjects>> {
        self.log_objects
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
            .and_then(std::sync::Weak::upgrade)
            .ok_or_else(|| anyhow::anyhow!("CI object storage is not configured"))
    }
    /// Acquisitions that have yielded, including connection creation or health checks.
    pub fn waiters(&self) -> usize {
        self.waiting.load(Ordering::Relaxed)
    }

    pub fn acquire(
        &self,
    ) -> impl Future<Output = Result<PoolConnection<DB>, Error>> + Send + 'static {
        let pool = self.inner.clone();
        let mut waiter = Waiter {
            count: self.waiting.clone(),
            waiting: false,
        };
        async move {
            let mut acquisition = pin!(pool.acquire());
            poll_fn(|cx| {
                let result = acquisition.as_mut().poll(cx);
                if result.is_pending() && !waiter.waiting {
                    waiter.count.fetch_add(1, Ordering::Relaxed);
                    waiter.waiting = true;
                }
                result
            })
            .await
        }
    }
    pub async fn begin(&self) -> Result<Transaction<'static, DB>, Error> {
        Transaction::begin(
            MaybePoolConnection::PoolConnection(self.acquire().await?),
            None,
        )
        .await
    }
    pub async fn begin_with(
        &self,
        statement: impl Into<Cow<'static, str>>,
    ) -> Result<Transaction<'static, DB>, Error> {
        Transaction::begin(
            MaybePoolConnection::PoolConnection(self.acquire().await?),
            Some(statement.into()),
        )
        .await
    }
    pub async fn connect(url: &str) -> Result<Self, Error> {
        sqlx::Pool::connect(url).await.map(Into::into)
    }
}

impl<'p, DB: Database> Executor<'p> for &Pool<DB>
where
    for<'c> &'c mut DB::Connection: Executor<'c, Database = DB>,
{
    type Database = DB;
    fn fetch_many<'e, 'q: 'e, E>(
        self,
        query: E,
    ) -> BoxStream<'e, Result<Either<DB::QueryResult, DB::Row>, Error>>
    where
        E: 'q + Execute<'q, DB>,
    {
        let pool = self.clone();
        // Drive the owned query future from the returned stream, without a detached task
        // or a collected result set. One queued row bounds backpressure and dropping the
        // stream drops its producer and connection together.
        let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
        let mut producer: Option<BoxFuture<'e, ()>> = Some(Box::pin(async move {
            let mut connection = match pool.acquire().await {
                Ok(connection) => connection,
                Err(error) => {
                    let _ = sender.send(Err(error)).await;
                    return;
                }
            };
            let mut rows = connection.fetch_many(query);
            while let Some(row) = rows.next().await {
                let failed = row.is_err();
                if sender.send(row).await.is_err() || failed {
                    break;
                }
            }
        }));
        Box::pin(futures_util::stream::poll_fn(move |cx| {
            if producer
                .as_mut()
                .is_some_and(|producer| producer.as_mut().poll(cx).is_ready())
            {
                producer = None;
            }
            receiver.poll_recv(cx)
        }))
    }
    fn fetch_optional<'e, 'q: 'e, E>(
        self,
        query: E,
    ) -> BoxFuture<'e, Result<Option<DB::Row>, Error>>
    where
        E: 'q + Execute<'q, DB>,
    {
        let pool = self.clone();
        Box::pin(async move { pool.acquire().await?.fetch_optional(query).await })
    }
    fn prepare_with<'e, 'q: 'e>(
        self,
        sql: &'q str,
        parameters: &'e [DB::TypeInfo],
    ) -> BoxFuture<'e, Result<DB::Statement<'q>, Error>> {
        let pool = self.clone();
        Box::pin(async move { pool.acquire().await?.prepare_with(sql, parameters).await })
    }
    fn describe<'e, 'q: 'e>(self, sql: &'q str) -> BoxFuture<'e, Result<Describe<DB>, Error>> {
        let pool = self.clone();
        Box::pin(async move { pool.acquire().await?.describe(sql).await })
    }
}

impl<'a, DB: Database> sqlx::Acquire<'a> for &Pool<DB> {
    type Database = DB;
    type Connection = PoolConnection<DB>;
    fn acquire(self) -> BoxFuture<'a, Result<Self::Connection, Error>> {
        Box::pin(Pool::acquire(self))
    }
    fn begin(self) -> BoxFuture<'a, Result<Transaction<'a, DB>, Error>> {
        let pool = self.clone();
        Box::pin(async move { pool.begin().await })
    }
}

// Directory-enrolled cells use PostgreSQL. SQLite remains a standalone cell and
// must be copied to PostgreSQL before joining a multi-cell deployment.
impl Pool<sqlx::Postgres> {
    pub fn set_directory(&self, provider: crate::cells::directory::Provider) {
        *self
            .directory
            .write()
            .unwrap_or_else(|error| error.into_inner()) = Some(provider);
    }
    pub fn set_search(&self, provider: crate::cells::search::Provider) {
        *self
            .search
            .write()
            .unwrap_or_else(|error| error.into_inner()) = Some(provider);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{task::Poll, time::Duration};
    async fn cancellation(pool: SqlitePool) {
        let held = pool.acquire().await.unwrap();
        let mut query = Box::pin(sqlx::query_scalar::<_, i64>("SELECT 1").fetch_one(&pool));
        poll_fn(|cx| {
            assert!(query.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        assert_eq!(pool.waiters(), 1);
        assert_eq!(pool.clone().waiters(), 1);
        drop(query);
        assert_eq!(
            pool.waiters(),
            0,
            "cancelled acquisition released its waiter"
        );
        let result = sqlx::query("SELECT 1").execute(&pool).await;
        assert!(matches!(result, Err(Error::PoolTimedOut)));
        assert_eq!(
            pool.waiters(),
            0,
            "timed-out acquisition released its waiter"
        );
        let mut success = Box::pin(sqlx::query_scalar::<_, i64>("SELECT 2").fetch_one(&pool));
        poll_fn(|cx| {
            assert!(success.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        assert_eq!(pool.waiters(), 1);
        drop(held);
        assert_eq!(success.await.unwrap(), 2);
        assert_eq!(pool.waiters(), 0);
        pool.close().await;
        assert!(matches!(pool.acquire().await, Err(Error::PoolClosed)));
        assert_eq!(pool.waiters(), 0);
    }
    #[tokio::test]
    async fn sqlite_waiters_are_shared_and_cleaned_on_success_timeout_cancel_and_close() {
        let raw = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .acquire_timeout(Duration::from_millis(100))
            .connect("sqlite::memory:")
            .await
            .unwrap();
        cancellation(raw.into()).await;
    }
    #[tokio::test]
    async fn postgres_waiters_are_shared_and_cleaned_on_success_timeout_cancel_and_close() {
        let url = std::env::var("GITSERV_TEST_DATABASE_URL")
            .expect("builder-test.sh supplies PostgreSQL");
        let pool: PgPool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(Duration::from_millis(200))
            .connect(&url)
            .await
            .unwrap()
            .into();
        let held = pool.acquire().await.unwrap();
        let mut query = Box::pin(sqlx::query_scalar::<_, i64>("SELECT 1::bigint").fetch_one(&pool));
        poll_fn(|cx| {
            assert!(query.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        assert_eq!(pool.waiters(), 1);
        assert_eq!(pool.clone().waiters(), 1);
        drop(query);
        assert_eq!(pool.waiters(), 0);
        assert!(matches!(pool.acquire().await, Err(Error::PoolTimedOut)));
        assert_eq!(pool.waiters(), 0);
        let mut success =
            Box::pin(sqlx::query_scalar::<_, i64>("SELECT 2::bigint").fetch_one(&pool));
        poll_fn(|cx| {
            assert!(success.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        assert_eq!(pool.waiters(), 1);
        drop(held);
        assert_eq!(success.await.unwrap(), 2);
        assert_eq!(pool.waiters(), 0);
        pool.close().await;
        assert!(matches!(pool.acquire().await, Err(Error::PoolClosed)));
        assert_eq!(pool.waiters(), 0);
    }
    #[tokio::test]
    async fn streaming_owns_its_connection_and_releases_it_when_dropped() {
        let pool: SqlitePool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap()
            .into();
        let sql = String::from("SELECT 1 AS value; SELECT 2 AS value;");
        let mut stream = sqlx::raw_sql(&sql).fetch_many(&pool);
        use sqlx::Row;
        let mut values = Vec::new();
        while let Some(row) = stream.next().await {
            if let Either::Right(row) = row.unwrap() {
                values.push(row.get::<i64, _>("value"));
            }
        }
        assert_eq!(values, [1, 2]);
        drop(stream);
        let sql = String::from("SELECT 3 AS value; SELECT 4 AS value;");
        let mut stream = sqlx::raw_sql(&sql).fetch_many(&pool);
        assert!(stream.next().await.unwrap().is_ok());
        assert_eq!(pool.num_idle(), 0);
        drop(stream);
        tokio::time::timeout(Duration::from_secs(2), pool.acquire())
            .await
            .unwrap()
            .unwrap();
        pool.close().await;
    }
}
