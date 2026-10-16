//! PostgreSQL side of a database copy.

use super::super::postgres::migrations::validate_history;
use super::{quote, Catalog, Column, Sequence, Table};
use anyhow::{bail, Result};
use sqlx::{Postgres, Transaction};

pub(super) async fn catalog(tx: &mut Transaction<'static, Postgres>) -> Result<Catalog> {
    let has_history: bool =
        sqlx::query_scalar("SELECT to_regclass('_gitserv_migrations') IS NOT NULL")
            .fetch_one(&mut **tx)
            .await?;
    let applied: Vec<(i64, String)> = if has_history {
        sqlx::query_as("SELECT version, sha256 FROM _gitserv_migrations ORDER BY version")
            .fetch_all(&mut **tx)
            .await?
    } else {
        Vec::new()
    };
    validate_history(&applied)?;
    let migrations = applied.into_iter().map(|(version, _)| version).collect();
    let names: Vec<String> = sqlx::query_scalar(
        "SELECT c.relname::text FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = current_schema() AND c.relkind = 'r' AND c.relname <> '_gitserv_migrations' \
         ORDER BY c.relname",
    )
    .fetch_all(&mut **tx)
    .await?;
    let mut tables = Vec::with_capacity(names.len());
    for name in names {
        let columns: Vec<(String, String)> = sqlx::query_as(
            "SELECT a.attname::text, format_type(a.atttypid, NULL) FROM pg_attribute a \
             WHERE a.attrelid = format('%I', $1)::regclass AND a.attnum > 0 AND NOT a.attisdropped \
             ORDER BY a.attnum",
        )
        .bind(&name)
        .fetch_all(&mut **tx)
        .await?;
        tables.push(Table {
            name,
            columns: columns
                .into_iter()
                .map(|(name, ty)| Column { name, ty })
                .collect(),
        });
    }
    Ok(Catalog { migrations, tables })
}

pub(super) async fn require_local_repositories(
    tx: &mut Transaction<'static, Postgres>,
) -> Result<()> {
    let placed: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM repositories WHERE storage_node_id IS NOT NULL)",
    )
    .fetch_one(&mut **tx)
    .await?;
    if placed {
        bail!("repositories have cluster placement; consolidate repository storage locally and remove placement (storage_node_id) before copying to SQLite");
    }
    Ok(())
}

/// Each serial column's sequence, read from the sequence itself. Its position still counts
/// ids issued to rows that were later deleted, which the surviving rows cannot show.
pub(super) async fn read_sequences(
    tx: &mut Transaction<'static, Postgres>,
) -> Result<Vec<Sequence>> {
    let mut sequences = Vec::new();
    for (table, column) in serial_columns(tx).await? {
        let name: Option<String> = sqlx::query_scalar("SELECT pg_get_serial_sequence($1, $2)")
            .bind(quote(&table))
            .bind(&column)
            .fetch_one(&mut **tx)
            .await?;
        let Some(name) = name else {
            continue;
        };
        let (last, called): (i64, bool) =
            sqlx::query_as(&format!("SELECT last_value, is_called FROM {name}"))
                .fetch_one(&mut **tx)
                .await?;
        sequences.push(Sequence {
            table,
            next: if called { last + 1 } else { last },
        });
    }
    Ok(sequences)
}

/// (table, column) for every column whose default draws from a sequence.
async fn serial_columns(tx: &mut Transaction<'static, Postgres>) -> Result<Vec<(String, String)>> {
    Ok(sqlx::query_as(
        "SELECT table_name::text, column_name::text FROM information_schema.columns \
         WHERE table_schema = current_schema() AND column_default LIKE 'nextval(%'",
    )
    .fetch_all(&mut **tx)
    .await?)
}

/// One JSON object per row, with every value as text in the form SQLite stores it:
/// timestamps in the application's RFC 3339 microseconds, INET as `address/prefix`,
/// blobs as hex, booleans as 0 or 1 and arrays as JSON.
pub(super) fn read_sql(table: &Table) -> String {
    let fields = table
        .columns
        .iter()
        .map(|column| {
            format!(
                "'{}', {}",
                column.name,
                read_value(&quote(&column.name), &column.ty)
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "SELECT json_build_object({fields})::text FROM {}",
        quote(&table.name)
    )
}

fn read_value(column: &str, ty: &str) -> String {
    match ty {
        "timestamp with time zone" => {
            format!(r#"to_char({column} AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS.US"Z"')"#)
        }
        "inet" => format!("host({column}) || '/' || masklen({column})"),
        "bytea" => format!("encode({column}, 'hex')"),
        "uuid" => format!("replace({column}::text, '-', '')"),
        "boolean" => format!("{column}::integer::text"),
        _ if ty.ends_with("[]") => format!("to_json({column})::text"),
        _ => format!("{column}::text"),
    }
}

/// Inserts from `jsonb_to_recordset` with every column staged as text, then casts each
/// value to its column type, so PostgreSQL does the parsing.
pub(super) fn insert_sql(table: &Table) -> String {
    let columns = table
        .columns
        .iter()
        .map(|column| quote(&column.name))
        .collect::<Vec<_>>()
        .join(", ");
    let staged = table
        .columns
        .iter()
        .map(|column| format!("{} text", quote(&column.name)))
        .collect::<Vec<_>>()
        .join(", ");
    let values = table
        .columns
        .iter()
        .map(|column| write_value(&format!("staged.{}", quote(&column.name)), &column.ty))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "INSERT INTO {} ({columns}) SELECT {values} FROM jsonb_to_recordset($1::jsonb) AS staged({staged})",
        quote(&table.name)
    )
}

fn write_value(value: &str, ty: &str) -> String {
    match ty {
        "bytea" => format!("decode({value}, 'hex')"),
        _ if ty.ends_with("[]") => format!(
            "CASE WHEN {value} IS NULL THEN NULL ELSE CAST(ARRAY(SELECT jsonb_array_elements_text({value}::jsonb)) AS {ty}) END"
        ),
        _ => format!("CAST({value} AS {ty})"),
    }
}

/// Drops the foreign keys, disables user triggers and returns the statements that put
/// them back after the rows are in, plus the statements that set each sequence past the
/// copied ids. Re-adding a foreign key checks every copied row, which is how a PostgreSQL
/// load gets referential checks without an ordering: the schema's references form cycles
/// (users and files), so no parent-first order exists.
/// User triggers maintain counters and visibility flags; the copied rows already hold
/// the source's values, so the triggers must not run. Disabling user triggers needs
/// table ownership, not superuser.
pub(super) async fn prepare_load(
    tx: &mut Transaction<'static, Postgres>,
    tables: &[Table],
    sequences: &[Sequence],
) -> Result<Vec<String>> {
    let mut restore = Vec::new();
    let constraints: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT con.conname::text, cl.relname::text, pg_get_constraintdef(con.oid) \
         FROM pg_constraint con JOIN pg_class cl ON cl.oid = con.conrelid \
         JOIN pg_namespace n ON n.oid = cl.relnamespace \
         WHERE con.contype = 'f' AND n.nspname = current_schema() ORDER BY cl.relname, con.conname",
    )
    .fetch_all(&mut **tx)
    .await?;
    for (name, table, definition) in constraints {
        sqlx::query(&format!(
            "ALTER TABLE {} DROP CONSTRAINT {}",
            quote(&table),
            quote(&name)
        ))
        .execute(&mut **tx)
        .await?;
        restore.push(format!(
            "ALTER TABLE {} ADD CONSTRAINT {} {definition}",
            quote(&table),
            quote(&name)
        ));
    }
    for table in tables {
        sqlx::query(&format!(
            "ALTER TABLE {} DISABLE TRIGGER USER",
            quote(&table.name)
        ))
        .execute(&mut **tx)
        .await?;
        restore.push(format!(
            "ALTER TABLE {} ENABLE TRIGGER USER",
            quote(&table.name)
        ));
    }
    for (table, column) in serial_columns(tx).await? {
        // The source's position, unless the copied rows already reach past it.
        let next = sequences
            .iter()
            .find(|sequence| sequence.table == table)
            .map_or(1, |sequence| sequence.next);
        restore.push(format!(
            "SELECT setval(pg_get_serial_sequence('{}', '{column}'), GREATEST(COALESCE(MAX({}), 0) + 1, {next}), false) FROM {}",
            quote(&table),
            quote(&column),
            quote(&table)
        ));
    }
    Ok(restore)
}
