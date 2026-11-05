use anyhow::Result;
use codeyard_db_core::pool::SqlitePool;
use sqlx::QueryBuilder;
use sqlx::Sqlite;

pub async fn release_note_rows(
    pool: &SqlitePool,
    repo_id: i64,
    commits: &[String],
) -> Result<Vec<(i32, String, String, Option<String>)>> {
    let mut rows = Vec::new();
    for chunk in commits.chunks(500) {
        let mut query=QueryBuilder::<Sqlite>::new("SELECT p.number,p.title,u.username,l.name FROM pull_requests p JOIN users u ON u.id=p.author_id LEFT JOIN pr_labels pl ON pl.pull_request_id=p.id LEFT JOIN labels l ON l.id=pl.label_id AND l.repo_id=p.repo_id WHERE p.repo_id=");
        query
            .push_bind(repo_id)
            .push(" AND p.merged AND p.merge_commit_sha IN (");
        let mut values = query.separated(",");
        for sha in chunk {
            values.push_bind(sha);
        }
        values.push_unseparated(") ORDER BY p.number,l.name LIMIT 20001");
        rows.extend(
            query
                .build_query_as::<(i32, String, String, Option<String>)>()
                .fetch_all(pool)
                .await?,
        );
        anyhow::ensure!(rows.len() <= 20000, "Too many labels for release notes");
    }
    Ok(rows)
}
