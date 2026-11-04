use crate::backend::db::discussion_search::Results;
use crate::backend::db::discussion_search::Search;
use crate::backend::db::discussion_search::Sort;
use anyhow::Result;
use sqlx::PgPool;

pub async fn search(
    pool: &PgPool,
    viewer: Option<i64>,
    query: &Search,
    sort: Sort,
    ascending: bool,
    offset: i64,
    limit: i64,
) -> Result<Results> {
    anyhow::ensure!(
        (0..=1_000_000).contains(&offset) && (0..=100).contains(&limit),
        "Invalid search window"
    );
    let statement = query
        .statement(
            true,
            &crate::backend::db::postgres::repositories::access::readable_condition(),
        )
        .map_err(anyhow::Error::msg)?;
    let mut tx =
        crate::backend::db::postgres::repositories::discovery::search_connection(pool).await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
        .execute(&mut *tx)
        .await?;
    let count_sql = format!("{} SELECT COUNT(*) FROM matched", statement.prefix);
    let mut count = sqlx::query_scalar::<_, i64>(&count_sql).bind(viewer);
    for value in &statement.args {
        count = count.bind(value);
    }
    let total = count.fetch_one(&mut *tx).await?;
    let sql=format!("{} SELECT i.id,i.repo_id,i.pull_request,CAST(i.score AS BIGINT) FROM matched i ORDER BY {} {},i.repo_id,i.number,i.pull_request,i.id LIMIT {} OFFSET {}",statement.prefix,sort.sql(),if ascending{"ASC"}else{"DESC"},statement.placeholder(statement.args.len()+2),statement.placeholder(statement.args.len()+3));
    let mut rows = sqlx::query_as::<_, (i64, i64, bool, i64)>(&sql).bind(viewer);
    for value in &statement.args {
        rows = rows.bind(value);
    }
    let items = rows.bind(limit).bind(offset).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(Results { total, items })
}
