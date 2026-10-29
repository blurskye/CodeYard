use crate::backend::db::catalog_search::{Hit, Options, Results, Search, Topic};
use anyhow::Result;
use sqlx::SqlitePool;
pub async fn search(
    pool: &SqlitePool,
    viewer: Option<i64>,
    query: &Search,
    options: &Options,
) -> Result<Results> {
    anyhow::ensure!(
        (0..1000).contains(&options.offset) && (1..=500).contains(&options.limit),
        "Invalid search window"
    );
    let statement = query
        .statement(
            false,
            &super::repositories::access::readable_condition(),
            options.repository,
        )
        .map_err(anyhow::Error::msg)?;
    let order = query
        .order(options.sort.as_deref(), options.ascending)
        .map_err(anyhow::Error::msg)?;
    let mut connection = super::repositories::discovery::search_connection(pool).await?;
    let result=async{sqlx::query("BEGIN").execute(&mut *connection).await?;
        let count_sql=format!("{} SELECT COUNT(*) FROM matched",statement.prefix);let mut count=sqlx::query_scalar::<_,i64>(&count_sql).bind(viewer);for value in &statement.args{count=count.bind(value);}let total=count.fetch_one(&mut *connection).await?;
        let sql=format!("{} SELECT m.key,m.repository_id,m.score FROM matched m ORDER BY {order} LIMIT {} OFFSET {}",statement.prefix,statement.placeholder(statement.args.len()+2),statement.placeholder(statement.args.len()+3));let mut rows=sqlx::query_as::<_,Hit>(&sql).bind(viewer);for value in &statement.args{rows=rows.bind(value);}let items=rows.bind(options.limit).bind(options.offset).fetch_all(&mut *connection).await?;sqlx::query("COMMIT").execute(&mut *connection).await?;Ok::<_,anyhow::Error>(Results{total,items})}.await;
    if result.is_err() {
        let _ = sqlx::query("ROLLBACK").execute(&mut *connection).await;
    }
    connection.finish().await?;
    result
}
pub async fn topic(pool: &SqlitePool, viewer: Option<i64>, name: &str) -> Result<Option<Topic>> {
    let mut connection = super::repositories::discovery::search_connection(pool).await?;
    let sql=format!("SELECT t.*,COUNT(*) AS repository_count FROM topics t JOIN repository_topics rt ON rt.topic=t.name JOIN repositories r ON r.id=rt.repo_id LEFT JOIN users u ON u.id=?1 WHERE t.name=?2 AND ({}) GROUP BY t.name",super::repositories::access::readable_condition());
    let result = sqlx::query_as(&sql)
        .bind(viewer)
        .bind(name)
        .fetch_optional(&mut *connection)
        .await;
    connection.finish().await?;
    Ok(result?)
}
