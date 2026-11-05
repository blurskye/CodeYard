//! Cell-local ID allocation. Operators initialize a stopped installation before registration.
use crate::{pool::PgPool, DbPool};
use anyhow::{ensure, Context, Result};
use codeyard_support::cells::Prefix;
use sqlx::PgConnection;
pub mod stubs;

/// Used by every native account/repository INSERT, including a cell with a later issuer range.
pub const LOCAL_HOME_SQL: &str = "COALESCE((SELECT cell_id FROM node_configuration WHERE id=1),0)";

pub async fn load(pool: &DbPool) -> Result<()> {
    let value: Option<(i32, i32)> = match pool {
        DbPool::Postgres(pool) => {
            sqlx::query_as("SELECT cell_id,cell_prefix FROM node_configuration WHERE id=1")
                .fetch_optional(pool)
                .await?
        }
        DbPool::Sqlite(pool) => {
            sqlx::query_as("SELECT cell_id,cell_prefix FROM node_configuration WHERE id=1")
                .fetch_optional(pool)
                .await?
        }
    };
    let (cell, prefix) = value.unwrap_or((0, 0));
    pool.set_cell_identity(
        Prefix::try_from(i64::from(cell))?,
        Prefix::try_from(i64::from(prefix))?,
    );
    Ok(())
}

/// The caller excludes serving processes with the existing maintenance and state locks.
/// Cell zero retains every ID and sequence position; a nonzero prefix needs an empty cell.
pub async fn initialize(pool: &DbPool, prefix: Prefix) -> Result<()> {
    let cell = match pool {
        DbPool::Postgres(pool) => initialize_postgres(pool, prefix, None).await?,
        DbPool::Sqlite(_) => {
            ensure!(prefix == Prefix::ZERO, "multiple cells require PostgreSQL");
            Prefix::ZERO
        }
    };
    pool.set_cell_identity(cell, prefix);
    Ok(())
}

/// The caller holds state/maintenance locks and obtains `prefix` from the pinned
/// directory using its live node key. Existing rows and stable ownership stay intact.
pub async fn renew(pool: &DbPool, previous: Prefix, prefix: Prefix) -> Result<()> {
    let DbPool::Postgres(postgres) = pool else {
        anyhow::bail!("multiple cells require PostgreSQL")
    };
    let cell = initialize_postgres(postgres, prefix, Some(previous)).await?;
    pool.set_cell_identity(cell, prefix);
    Ok(())
}

/// Read only the bounded sequence catalog, before reserving a successor externally.
pub async fn highest_id(pool: &DbPool) -> Result<i64> {
    let DbPool::Postgres(pool) = pool else {
        anyhow::bail!("multiple cells require PostgreSQL")
    };
    let mut connection = pool.acquire().await?;
    let mut maximum = 0;
    for (table, column) in serial_columns(&mut connection).await? {
        let sequence = sequence_name(&mut connection, &table, &column)
            .await?
            .context("cell IDs require an owned sequence")?;
        let (last, called): (i64, bool) =
            sqlx::query_as(&format!("SELECT last_value,is_called FROM {sequence}"))
                .fetch_one(&mut *connection)
                .await?;
        if called {
            maximum = maximum.max(last);
        }
    }
    Ok(maximum)
}

async fn initialize_postgres(
    pool: &PgPool,
    prefix: Prefix,
    renewal: Option<Prefix>,
) -> Result<Prefix> {
    let mut tx = pool.begin().await?;
    sqlx::query("INSERT INTO node_configuration(id) VALUES(1) ON CONFLICT(id) DO NOTHING")
        .execute(&mut *tx)
        .await?;
    let (cell, previous): (i32, i32) =
        sqlx::query_as("SELECT cell_id,cell_prefix FROM node_configuration WHERE id=1 FOR UPDATE")
            .fetch_one(&mut *tx)
            .await?;
    let cell = Prefix::try_from(i64::from(cell))?;
    let previous = Prefix::try_from(i64::from(previous))?;
    if let Some(expected) = renewal {
        ensure!(
            previous == expected && previous != prefix,
            "cell issuer changed during renewal"
        );
    } else {
        ensure!(
            previous == Prefix::ZERO || previous == prefix,
            "a cell's prefix cannot be replaced"
        );
    }
    let mut needs_prefix = false;
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
        if fresh && renewal.is_none() {
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
        if renewal.is_some() {
            ensure!(
                previous.ids().contains(&last),
                "sequence {table}.{column} is outside its cell range"
            );
            needs_prefix |= called && previous.needs_next_prefix(last);
        }
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
    ensure!(
        renewal.is_none() || needs_prefix,
        "no cell sequence has reached 80% use"
    );
    sqlx::query("UPDATE node_configuration SET cell_id=CASE WHEN $2 AND cell_prefix=0 THEN $1 ELSE cell_id END,cell_prefix=$1,setup_completed=CASE WHEN $1<>0 THEN true ELSE setup_completed END WHERE id=1")
        .bind(i32::from(prefix.get())).bind(renewal.is_none())
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(if renewal.is_none() && previous == Prefix::ZERO {
        prefix
    } else {
        cell
    })
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
