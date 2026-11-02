use crate::backend::db::catalog_search::{Hit, Options, Results, Search, Topic};
use anyhow::Result;
use sqlx::PgPool;
pub async fn search(
    pool: &PgPool,
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
            true,
            &super::repositories::access::readable_condition(),
            options.repository,
        )
        .map_err(anyhow::Error::msg)?;
    let order = query
        .order(options.sort.as_deref(), options.ascending)
        .map_err(anyhow::Error::msg)?;
    let mut tx = super::repositories::discovery::search_connection(pool).await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
        .execute(&mut *tx)
        .await?;
    let count_sql = format!("{} SELECT COUNT(*) FROM matched", statement.prefix);
    let mut count = sqlx::query_scalar::<_, i64>(&count_sql).bind(viewer);
    for value in &statement.args {
        count = count.bind(value);
    }
    let total = count.fetch_one(&mut *tx).await?;
    let sql=format!("{} SELECT m.key,m.repository_id,m.score FROM matched m ORDER BY {order} LIMIT {} OFFSET {}",statement.prefix,statement.placeholder(statement.args.len()+2),statement.placeholder(statement.args.len()+3));
    let mut rows = sqlx::query_as::<_, Hit>(&sql).bind(viewer);
    for value in &statement.args {
        rows = rows.bind(value);
    }
    let items = rows
        .bind(options.limit)
        .bind(options.offset)
        .fetch_all(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Results { total, items })
}
pub async fn topic(
    pool: &PgPool,
    viewer: Option<i64>,
    name: &str,
    policy_token: Option<i64>,
) -> Result<Option<Topic>> {
    let mut tx = super::repositories::discovery::search_connection(pool).await?;
    let mut statement = crate::backend::db::discussion_search::Statement::new(true);
    let readable = statement.repository_policy(
        &super::repositories::access::readable_condition(),
        policy_token,
    );
    let name = statement.bind(name);
    let sql=format!("SELECT t.*,COUNT(*) AS repository_count FROM topics t JOIN repository_topics rt ON rt.topic=t.name JOIN repositories r ON r.id=rt.repo_id LEFT JOIN users u ON u.id=$1 WHERE t.name={name} AND ({readable}) GROUP BY t.name");
    let mut query = sqlx::query_as(&sql).bind(viewer);
    for value in &statement.args {
        query = query.bind(value);
    }
    let topic = query.fetch_optional(&mut *tx).await?;
    tx.commit().await?;
    Ok(topic)
}
