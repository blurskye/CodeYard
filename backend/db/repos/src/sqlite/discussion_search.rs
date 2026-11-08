use crate::discussion_search::Results;
use crate::discussion_search::Search;
use crate::discussion_search::Sort;
use anyhow::Result;
use codeyard_database::pool::SqlitePool;

pub async fn search(
    pool: &SqlitePool,
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
            false,
            &codeyard_database::queries::sqlite::repositories::access::readable_condition(),
        )
        .map_err(anyhow::Error::msg)?;
    let mut connection = crate::sqlite::repositories::discovery::search_connection(pool).await?;
    sqlx::query("BEGIN").execute(&mut *connection).await?;
    let count_sql = format!("{} SELECT COUNT(*) FROM matched", statement.prefix);
    let mut count =
        sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(count_sql.as_str())).bind(viewer);
    for value in &statement.args {
        count = count.bind(value);
    }
    let total = count.fetch_one(&mut *connection).await?;
    let sql=format!("{} SELECT i.id,i.repo_id,i.pull_request,i.score FROM matched i ORDER BY {} {},i.repo_id,i.number,i.pull_request,i.id LIMIT {} OFFSET {}",statement.prefix,sort.sql(),if ascending{"ASC"}else{"DESC"},statement.placeholder(statement.args.len()+2),statement.placeholder(statement.args.len()+3));
    let mut rows =
        sqlx::query_as::<_, (i64, i64, bool, i64)>(sqlx::AssertSqlSafe(sql.as_str())).bind(viewer);
    for value in &statement.args {
        rows = rows.bind(value);
    }
    let items = rows
        .bind(limit)
        .bind(offset)
        .fetch_all(&mut *connection)
        .await?;
    sqlx::query("COMMIT").execute(&mut *connection).await?;
    connection.finish().await?;
    Ok(Results { total, items })
}
