//! SQLite side of a database copy.

use super::super::sqlite::migrations::validate_history;
use super::{literal, quote, Catalog, Column, Sequence, Table};
use anyhow::{bail, Result};
use sqlx::{Sqlite, Transaction};

pub(super) async fn catalog(tx: &mut Transaction<'static, Sqlite>) -> Result<Catalog> {
    let has_history: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = '_gitserv_sqlite_migrations'",
    )
    .fetch_one(&mut **tx)
    .await?;
    let applied: Vec<(i64, String)> = if has_history == 0 {
        Vec::new()
    } else {
        sqlx::query_as("SELECT version, sha256 FROM _gitserv_sqlite_migrations ORDER BY version")
            .fetch_all(&mut **tx)
            .await?
    };
    validate_history(&applied)?;
    let migrations = applied.into_iter().map(|(version, _)| version).collect();
    let names: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT GLOB 'sqlite_*' AND name NOT GLOB '_gitserv_*' ORDER BY name",
    )
    .fetch_all(&mut **tx)
    .await?;
    let mut tables = Vec::with_capacity(names.len());
    for name in names {
        let columns: Vec<(String, String)> =
            sqlx::query_as("SELECT name, type FROM pragma_table_info(?1) ORDER BY cid")
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

/// AUTOINCREMENT keeps each table's highest issued id in sqlite_sequence, and deleting
/// rows does not lower it. Tables that never held a row have no entry.
pub(super) async fn read_sequences(tx: &mut Transaction<'static, Sqlite>) -> Result<Vec<Sequence>> {
    let has_table: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = 'sqlite_sequence'",
    )
    .fetch_one(&mut **tx)
    .await?;
    if has_table == 0 {
        return Ok(Vec::new());
    }
    let issued: Vec<(String, i64)> = sqlx::query_as("SELECT name, seq FROM sqlite_sequence")
        .fetch_all(&mut **tx)
        .await?;
    Ok(issued
        .into_iter()
        .map(|(table, last)| Sequence {
            table,
            next: last + 1,
        })
        .collect())
}

/// One JSON object per row. Blobs become hex text; every other value becomes text,
/// which the destination converts to the column's type.
pub(super) fn read_sql(table: &Table) -> String {
    let fields = table
        .columns
        .iter()
        .map(|column| {
            let name = quote(&column.name);
            format!(
                "'{}', CASE WHEN typeof({name}) = 'blob' THEN hex({name}) ELSE CAST({name} AS TEXT) END",
                column.name
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!("SELECT json_object({fields}) FROM {}", quote(&table.name))
}

/// Loads a JSON array of row objects. Blob columns receive hex text and are decoded
/// here; the other values are passed through as text.
pub(super) fn insert_sql(table: &Table) -> String {
    let columns = table
        .columns
        .iter()
        .map(|column| quote(&column.name))
        .collect::<Vec<_>>()
        .join(", ");
    let values = table
        .columns
        .iter()
        .map(|column| {
            let value = format!("json_extract(value, '$.\"{}\"')", column.name);
            if column.ty.eq_ignore_ascii_case("blob") {
                format!("unhex({value})")
            } else {
                value
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "INSERT INTO {} ({columns}) SELECT {values} FROM json_each(?1)",
        quote(&table.name)
    )
}

/// Defers foreign-key checks to the transaction's end, so the tables load in any
/// order (the schema's references form cycles). Triggers keep counters and visibility
/// flags; they are dropped for the load and the returned statements recreate them.
/// The returned statements also set each counter to the source's position. They run
/// after the rows load, because loading an explicit id moves the counter up.
pub(super) async fn prepare_load(
    tx: &mut Transaction<'static, Sqlite>,
    sequences: &[Sequence],
) -> Result<Vec<String>> {
    sqlx::query("PRAGMA defer_foreign_keys = ON")
        .execute(&mut **tx)
        .await?;
    let triggers: Vec<(String, String)> =
        sqlx::query_as("SELECT name, sql FROM sqlite_master WHERE type = 'trigger'")
            .fetch_all(&mut **tx)
            .await?;
    let mut restore = Vec::with_capacity(triggers.len() + 2 * sequences.len());
    for (name, sql) in triggers {
        sqlx::query(&format!("DROP TRIGGER {}", quote(&name)))
            .execute(&mut **tx)
            .await?;
        restore.push(sql);
    }
    for sequence in sequences {
        restore.push(format!(
            "DELETE FROM sqlite_sequence WHERE name = {}",
            literal(&sequence.table)
        ));
        restore.push(format!(
            "INSERT INTO sqlite_sequence (name, seq) VALUES ({}, {})",
            literal(&sequence.table),
            sequence.next - 1
        ));
    }
    Ok(restore)
}

/// PostgreSQL text cannot hold NUL, and SQLite text can. Refuses the copy before anything
/// is written, naming the first row that holds one. Blobs are copied as hex, so only text
/// values can break the load.
pub(super) async fn check_text_without_nul(
    tx: &mut Transaction<'static, Sqlite>,
    tables: &[Table],
) -> Result<()> {
    for table in tables {
        let tests = table
            .columns
            .iter()
            .map(|column| {
                let name = quote(&column.name);
                format!("(typeof({name}) = 'text' AND instr(CAST({name} AS BLOB), X'00') > 0)")
            })
            .collect::<Vec<_>>()
            .join(" OR ");
        let row: Option<i64> = sqlx::query_scalar(&format!(
            "SELECT rowid FROM {} WHERE {tests} LIMIT 1",
            quote(&table.name)
        ))
        .fetch_optional(&mut **tx)
        .await?;
        if let Some(row) = row {
            bail!(
                "table {} row {row} contains a NUL character, which PostgreSQL text cannot store; nothing was copied. Remove the NUL from that row in the SQLite database and copy again.",
                table.name
            );
        }
    }
    Ok(())
}

pub(super) async fn check_foreign_keys(tx: &mut Transaction<'static, Sqlite>) -> Result<()> {
    let violations = sqlx::query("PRAGMA foreign_key_check")
        .fetch_all(&mut **tx)
        .await?;
    if !violations.is_empty() {
        bail!("copied rows break a foreign key, nothing was committed");
    }
    Ok(())
}
