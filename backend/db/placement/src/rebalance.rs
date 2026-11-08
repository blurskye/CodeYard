//! One bounded copy-then-promote move per participating node.
use anyhow::{ensure, Result};
use codeyard_database::{native_operation, DbPool};

pub struct Move {
    pub repository: i64,
    pub source: i64,
    pub target: i64,
    pub targets: Vec<i64>,
    pub terms: Vec<(i64, i64)>,
}

pub(crate) fn decode(row: (i64, i64, i64, String, String)) -> Result<Move> {
    ensure!(
        row.3.len() <= 32 * 1024 && row.4.len() <= 1024 * 1024,
        "move record limit exceeded"
    );
    let targets: Vec<i64> = serde_json::from_str(&row.3)?;
    let terms: Vec<(i64, i64)> = serde_json::from_str(&row.4)?;
    ensure!(
        !targets.is_empty()
            && targets.len() <= 999
            && !terms.is_empty()
            && terms.len() as i64 <= crate::drain::repository_limit()?,
        "move record limit exceeded"
    );
    ensure!(
        row.0 > 0
            && row.1 > 0
            && row.2 > 0
            && row.1 != row.2
            && targets.contains(&row.2)
            && !targets.contains(&row.1)
            && targets.iter().all(|id| *id > 0)
            && terms.iter().all(|(id, term)| *id > 0 && *term >= 0),
        "invalid move record"
    );
    Ok(Move {
        repository: row.0,
        source: row.1,
        target: row.2,
        targets,
        terms,
    })
}

native_operation!(crate::postgres::rebalance::plan, crate::sqlite::rebalance::plan,
    fn plan(pool: &DbPool, node: i64) -> Result<Option<Move>>);
native_operation!(crate::postgres::rebalance::retire_source, crate::sqlite::rebalance::retire_source,
    /// Retires database assignments only after every replacement is current and serving.
    fn retire_source(pool: &DbPool, movement: &Move) -> Result<bool>);
native_operation!(crate::postgres::rebalance::finish, crate::sqlite::rebalance::finish,
    fn finish(pool: &DbPool, movement: &Move) -> Result<bool>);
