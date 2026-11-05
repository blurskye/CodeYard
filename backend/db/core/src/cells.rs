//! Cell-local ID allocation. Operators initialize a stopped installation before registration.
use crate::{pool::PgPool, DbPool};
use anyhow::{ensure, Context, Result};
use codeyard_support::cells::Prefix;
use sqlx::PgConnection;

pub async fn load(pool: &DbPool) -> Result<()> {
    let value: Option<i32> = match pool {
        DbPool::Postgres(pool) => {
            sqlx::query_scalar("SELECT cell_prefix FROM node_configuration WHERE id=1")
                .fetch_optional(pool)
                .await?
        }
        DbPool::Sqlite(pool) => {
            sqlx::query_scalar("SELECT cell_prefix FROM node_configuration WHERE id=1")
                .fetch_optional(pool)
                .await?
        }
    };
    pool.set_cell_prefix(Prefix::try_from(i64::from(value.unwrap_or(0)))?);
    Ok(())
}

/// The caller excludes serving processes with the existing maintenance and state locks.
/// Cell zero retains every ID and sequence position; a nonzero prefix needs an empty cell.
pub async fn initialize(pool: &DbPool, prefix: Prefix) -> Result<()> {
    match pool {
        DbPool::Postgres(pool) => initialize_postgres(pool, prefix).await?,
        DbPool::Sqlite(_) => ensure!(prefix == Prefix::ZERO, "multiple cells require PostgreSQL"),
    }
    pool.set_cell_prefix(prefix);
    Ok(())
}

async fn initialize_postgres(pool: &PgPool, prefix: Prefix) -> Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query("INSERT INTO node_configuration(id) VALUES(1) ON CONFLICT(id) DO NOTHING")
        .execute(&mut *tx)
        .await?;
    let previous: i32 =
        sqlx::query_scalar("SELECT cell_prefix FROM node_configuration WHERE id=1 FOR UPDATE")
            .fetch_one(&mut *tx)
            .await?;
    let previous = Prefix::try_from(i64::from(previous))?;
    ensure!(
        previous == Prefix::ZERO || previous == prefix,
        "a cell's prefix cannot be replaced"
    );
    let fresh = previous != prefix;
    let columns = serial_columns(&mut tx).await?;
    let bounds = prefix.ids();
    for (table, column) in columns {
        let name = sequence_name(&mut tx, &table, &column)
            .await?
            .context("cell IDs require an owned sequence")?;
        let quoted: String = sqlx::query_scalar("SELECT format('%I.%I',current_schema(),$1)")
            .bind(&table)
            .fetch_one(&mut *tx)
            .await?;
        // CLI writers must also finish before the emptiness check; maintenance excludes
        // servers, and these table locks close the remaining allocation race.
        sqlx::query(&format!("LOCK TABLE {quoted} IN ACCESS EXCLUSIVE MODE"))
            .execute(&mut *tx)
            .await?;
        if fresh {
            let populated: bool =
                sqlx::query_scalar(&format!("SELECT EXISTS(SELECT 1 FROM {quoted} LIMIT 1)"))
                    .fetch_one(&mut *tx)
                    .await?;
            ensure!(
                !populated,
                "a new prefix requires an empty cell; existing table {table} has rows"
            );
        }
        let (last, called): (i64, bool) =
            sqlx::query_as(&format!("SELECT last_value,is_called FROM {name}"))
                .fetch_one(&mut *tx)
                .await?;
        if !fresh {
            ensure!(
                bounds.contains(&last),
                "sequence {table}.{column} is outside its cell range"
            );
            ensure!(
                !called || last < *bounds.end(),
                "sequence {table}.{column} needs another prefix"
            );
        }
        let restart = if fresh {
            format!(" RESTART WITH {}", bounds.start())
        } else {
            String::new()
        };
        // Names come from PostgreSQL's identifier formatter, numbers from the typed prefix.
        sqlx::query(&format!("ALTER SEQUENCE {name} AS bigint MINVALUE {} MAXVALUE {} START WITH {} NO CYCLE{restart}",
            bounds.start(), bounds.end(), bounds.start()))
            .execute(&mut *tx).await?;
    }
    sqlx::query("UPDATE node_configuration SET cell_prefix=$1,setup_completed=CASE WHEN $1<>0 THEN true ELSE setup_completed END WHERE id=1")
        .bind(i32::from(prefix.get()))
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

/// The same bounded catalog drives range allocation and whole-database copy.
pub async fn serial_columns(connection: &mut PgConnection) -> Result<Vec<(String, String)>> {
    let columns: Vec<(String, String)> = sqlx::query_as(
        "SELECT table_name::text,column_name::text FROM information_schema.columns WHERE table_schema=current_schema() AND (column_default LIKE 'nextval(%' OR is_identity='YES') ORDER BY table_name,column_name LIMIT 1025",
    ).fetch_all(connection).await?;
    ensure!(
        columns.len() <= 1024,
        "cell sequence catalog exceeds its limit"
    );
    Ok(columns)
}

pub async fn sequence_name(
    connection: &mut PgConnection,
    table: &str,
    column: &str,
) -> Result<Option<String>> {
    Ok(
        sqlx::query_scalar("SELECT pg_get_serial_sequence(format('%I.%I',current_schema(),$1),$2)")
            .bind(table)
            .bind(column)
            .fetch_one(connection)
            .await?,
    )
}
