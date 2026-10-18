//! Copy a whole application database into an empty database of the other engine.
//!
//! The source is read inside one snapshot transaction. The destination is migrated,
//! checked for emptiness, loaded and verified inside one transaction that commits only
//! when every table's row count matches. Engine specifics live in `copy/sqlite.rs` and
//! `copy/postgres.rs`; the table and column lists come from each database's catalog.

mod postgres;
mod sqlite;

use super::{connection, init_db, run_migrations, DbPool};
use anyhow::{bail, Result};
use futures::{Stream, TryStreamExt};
use sqlx::{Postgres, Sqlite, Transaction};

/// Rows the baseline migration inserts. A migrated destination that was never used
/// may hold exactly these rows, and the copy replaces them with the source's rows.
const MIGRATION_SEEDS: &[(&str, &str, &[&str])] = &[(
    "file_reason_limits",
    "reason",
    &[
        "avatar",
        "org_avatar",
        "release_asset",
        "issue_attachment",
        "pr_attachment",
        "readme_image",
        "repo_asset",
        "other",
    ],
)];

/// Rows travel as JSON arrays of about this many bytes, so a large table never sits in
/// memory whole.
const CHUNK_BYTES: usize = 4 << 20;

#[derive(PartialEq, Eq)]
enum Engine {
    Sqlite,
    Postgres,
}

struct Column {
    name: String,
    /// The SQLite declared type, or the PostgreSQL type without modifiers.
    ty: String,
}

struct Table {
    name: String,
    columns: Vec<Column>,
}

struct Catalog {
    migrations: Vec<i64>,
    tables: Vec<Table>,
}

/// Where a table's id counter stands: the id it would issue next. Deleting a row does not
/// lower this, so it can be above every id still in the table.
struct Sequence {
    table: String,
    next: i64,
}

#[derive(Debug)]
pub struct TableCount {
    pub table: String,
    pub rows: i64,
}

/// One open transaction: the source snapshot or the destination load.
enum Tx {
    Sqlite(Transaction<'static, Sqlite>),
    Postgres(Transaction<'static, Postgres>),
}

/// Copies every table from `from` into `to`. Returns the copied row counts.
///
/// The server must be stopped while this runs; nothing enforces that here.
pub async fn copy_database(from: &str, to: &str) -> Result<Vec<TableCount>> {
    if engine_of(from)? == engine_of(to)? {
        bail!("source and destination must use different engines: SQLite to PostgreSQL, or PostgreSQL to SQLite");
    }
    let source_pool = connection::open_source(from).await?;
    let mut source = snapshot(&source_pool).await?;
    let source_catalog = catalog(&mut source).await?;
    match &mut source {
        Tx::Sqlite(tx) => sqlite::check_text_without_nul(tx, &source_catalog.tables).await?,
        Tx::Postgres(tx) => postgres::require_local_repositories(tx).await?,
    }
    let sequences = read_sequences(&mut source).await?;

    let destination_pool = init_db(to).await?;
    run_migrations(&destination_pool).await?;
    let mut destination = write(&destination_pool).await?;
    let destination_catalog = catalog(&mut destination).await?;
    if source_catalog.migrations != destination_catalog.migrations {
        bail!(
            "migration versions differ (source {:?}, destination {:?}): the baseline history does not match; nothing was copied",
            source_catalog.migrations,
            destination_catalog.migrations
        );
    }
    if shape(&source_catalog.tables) != shape(&destination_catalog.tables) {
        bail!("table or column lists differ between the databases; nothing was copied");
    }

    require_empty(&mut destination, &destination_catalog.tables).await?;
    let restore = prepare_load(&mut destination, &destination_catalog.tables, &sequences).await?;
    let mut counts = Vec::with_capacity(source_catalog.tables.len());
    // Both catalogs are sorted by name and hold the same names, so the tables pair up.
    for (table, target) in source_catalog
        .tables
        .iter()
        .zip(&destination_catalog.tables)
    {
        let rows = scalar(&mut source, &count_sql(&table.name)).await?;
        copy_table(&mut source, &mut destination, table, target).await?;
        counts.push(TableCount {
            table: table.name.clone(),
            rows,
        });
    }
    for statement in &restore {
        execute(&mut destination, statement).await?;
    }
    if let Tx::Sqlite(tx) = &mut destination {
        sqlite::check_foreign_keys(tx).await?;
    }
    let mut mismatched = Vec::new();
    for count in &counts {
        let copied = scalar(&mut destination, &count_sql(&count.table)).await?;
        if copied != count.rows {
            mismatched.push(format!("{} ({copied} of {})", count.table, count.rows));
        }
    }
    if !mismatched.is_empty() {
        bail!(
            "row counts differ after copy, nothing was committed: {}",
            mismatched.join(", ")
        );
    }
    destination.commit().await?;
    // The snapshot holds a connection until it ends, and closing the pool waits for it.
    drop(source);
    source_pool.close().await;
    destination_pool.close().await;
    Ok(counts)
}

fn engine_of(url: &str) -> Result<Engine> {
    if url.starts_with("postgres://") || url.starts_with("postgresql://") {
        Ok(Engine::Postgres)
    } else if url.starts_with("sqlite:") {
        Ok(Engine::Sqlite)
    } else {
        bail!("database URL must start with sqlite: or postgres:// (or postgresql://)")
    }
}

/// A read-only transaction. PostgreSQL gets a repeatable-read snapshot; SQLite's
/// deferred read transaction already sees one snapshot for all of its reads.
async fn snapshot(pool: &DbPool) -> Result<Tx> {
    match pool {
        DbPool::Sqlite(pool) => Ok(Tx::Sqlite(pool.begin().await?)),
        DbPool::Postgres(pool) => {
            let mut tx = pool.begin().await?;
            sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
                .execute(&mut *tx)
                .await?;
            Ok(Tx::Postgres(tx))
        }
    }
}

/// A write transaction. SQLite takes its write lock now rather than in the middle of
/// the load, where a busy lock would fail after rows were already written.
async fn write(pool: &DbPool) -> Result<Tx> {
    match pool {
        DbPool::Sqlite(pool) => Ok(Tx::Sqlite(pool.begin_with("BEGIN IMMEDIATE").await?)),
        DbPool::Postgres(pool) => {
            let mut tx = pool.begin().await?;
            // The load drops and re-adds foreign keys; fail rather than wait forever
            // when a server is still attached to the destination.
            sqlx::query("SET LOCAL lock_timeout = '15s'")
                .execute(&mut *tx)
                .await?;
            Ok(Tx::Postgres(tx))
        }
    }
}

impl Tx {
    async fn commit(self) -> Result<()> {
        match self {
            Tx::Sqlite(tx) => tx.commit().await?,
            Tx::Postgres(tx) => tx.commit().await?,
        }
        Ok(())
    }
}

async fn catalog(tx: &mut Tx) -> Result<Catalog> {
    let mut catalog = match tx {
        Tx::Sqlite(tx) => sqlite::catalog(tx).await?,
        Tx::Postgres(tx) => postgres::catalog(tx).await?,
    };
    // Sorted here: PostgreSQL's order depends on the collation, SQLite's does not.
    catalog.tables.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(catalog)
}

async fn read_sequences(tx: &mut Tx) -> Result<Vec<Sequence>> {
    match tx {
        Tx::Sqlite(tx) => sqlite::read_sequences(tx).await,
        Tx::Postgres(tx) => postgres::read_sequences(tx).await,
    }
}

async fn prepare_load(
    tx: &mut Tx,
    tables: &[Table],
    sequences: &[Sequence],
) -> Result<Vec<String>> {
    match tx {
        Tx::Sqlite(tx) => sqlite::prepare_load(tx, sequences).await,
        Tx::Postgres(tx) => postgres::prepare_load(tx, tables, sequences).await,
    }
}

/// Fails unless each table is empty. The seed rows named in `MIGRATION_SEEDS` are
/// allowed, and are deleted so the source's rows can take their place.
async fn require_empty(destination: &mut Tx, tables: &[Table]) -> Result<()> {
    for table in tables {
        let seed = MIGRATION_SEEDS
            .iter()
            .find(|(name, _, _)| *name == table.name);
        let unexpected = match seed {
            Some((_, key, values)) => {
                let list = values
                    .iter()
                    .map(|value| format!("'{value}'"))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!(
                    "SELECT count(*) FROM {} WHERE {} NOT IN ({list})",
                    quote(&table.name),
                    quote(key)
                )
            }
            None => count_sql(&table.name),
        };
        if scalar(destination, &unexpected).await? != 0 {
            bail!(
                "destination table {} already has rows; copy into an empty database",
                table.name
            );
        }
        if seed.is_some() {
            execute(destination, &format!("DELETE FROM {}", quote(&table.name))).await?;
        }
    }
    Ok(())
}

/// Reads the source table with its own column types and loads it with the
/// destination's, since the two catalogs describe the same columns differently.
async fn copy_table(source: &mut Tx, destination: &mut Tx, from: &Table, to: &Table) -> Result<()> {
    let sql = read_sql(source, from);
    match source {
        Tx::Sqlite(tx) => {
            let rows = sqlx::query_scalar::<_, String>(&sql).fetch(&mut **tx);
            load_rows(destination, to, rows).await
        }
        Tx::Postgres(tx) => {
            let rows = sqlx::query_scalar::<_, String>(&sql).fetch(&mut **tx);
            load_rows(destination, to, rows).await
        }
    }
}

/// Each source row is one JSON object of text values. Rows are sent in chunks, and
/// the destination converts each text value to its column type in SQL.
async fn load_rows<S>(destination: &mut Tx, table: &Table, mut rows: S) -> Result<()>
where
    S: Stream<Item = Result<String, sqlx::Error>> + Unpin,
{
    let insert = insert_sql(destination, table);
    let mut chunk = String::new();
    while let Some(row) = rows.try_next().await? {
        chunk.push(if chunk.is_empty() { '[' } else { ',' });
        chunk.push_str(&row);
        if chunk.len() >= CHUNK_BYTES {
            chunk.push(']');
            insert_chunk(destination, &insert, &chunk).await?;
            chunk.clear();
        }
    }
    if !chunk.is_empty() {
        chunk.push(']');
        insert_chunk(destination, &insert, &chunk).await?;
    }
    Ok(())
}

fn read_sql(source: &Tx, table: &Table) -> String {
    match source {
        Tx::Sqlite(_) => sqlite::read_sql(table),
        Tx::Postgres(_) => postgres::read_sql(table),
    }
}

fn insert_sql(destination: &Tx, table: &Table) -> String {
    match destination {
        Tx::Sqlite(_) => sqlite::insert_sql(table),
        Tx::Postgres(_) => postgres::insert_sql(table),
    }
}

async fn insert_chunk(destination: &mut Tx, sql: &str, chunk: &str) -> Result<()> {
    match destination {
        Tx::Sqlite(tx) => sqlx::query(sql)
            .bind(chunk)
            .execute(&mut **tx)
            .await
            .map(drop)?,
        Tx::Postgres(tx) => sqlx::query(sql)
            .bind(chunk)
            .execute(&mut **tx)
            .await
            .map(drop)?,
    }
    Ok(())
}

async fn execute(tx: &mut Tx, sql: &str) -> Result<()> {
    match tx {
        Tx::Sqlite(tx) => sqlx::query(sql).execute(&mut **tx).await.map(drop)?,
        Tx::Postgres(tx) => sqlx::query(sql).execute(&mut **tx).await.map(drop)?,
    }
    Ok(())
}

async fn scalar(tx: &mut Tx, sql: &str) -> Result<i64> {
    match tx {
        Tx::Sqlite(tx) => Ok(sqlx::query_scalar::<_, i64>(sql)
            .fetch_one(&mut **tx)
            .await?),
        Tx::Postgres(tx) => Ok(sqlx::query_scalar::<_, i64>(sql)
            .fetch_one(&mut **tx)
            .await?),
    }
}

fn count_sql(table: &str) -> String {
    format!("SELECT count(*) FROM {}", quote(table))
}

/// Table and column names, which must match on both sides. Column order is not compared:
/// the rows are matched by name.
fn shape(tables: &[Table]) -> Vec<(&str, Vec<&str>)> {
    tables
        .iter()
        .map(|table| {
            let mut columns: Vec<&str> = table
                .columns
                .iter()
                .map(|column| column.name.as_str())
                .collect();
            columns.sort_unstable();
            (table.name.as_str(), columns)
        })
        .collect()
}

/// A double-quoted SQL identifier. Both engines accept this form.
fn quote(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

/// A single-quoted SQL string literal.
fn literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}
