use super::models::Label;
use crate::backend::db::labels::{bounded_label_ids, ItemLabel, LabelTarget, LabelWriteRefusal};
use anyhow::Result;
use sqlx::{PgPool, Postgres, Transaction};

async fn begin_label_write(
    pool: &PgPool,
    actor_id: i64,
    repo_id: i64,
) -> Result<Transaction<'_, Postgres>> {
    let mut tx = pool.begin().await?;
    // CI singleton FIRST, before any issue/label lock. Retain authority to commit.
    anyhow::ensure!(
        super::discussions::lock_admin_authority(&mut tx, actor_id, repo_id).await?,
        LabelWriteRefusal::Denied
    );
    Ok(tx)
}

async fn lock_item_labels(
    tx: &mut Transaction<'_, Postgres>,
    repo_id: i64,
    target: LabelTarget,
    item_id: i64,
    label_ids: &[i64],
) -> Result<Vec<i64>> {
    let ids = bounded_label_ids(label_ids)?;
    let (table, _, _) = target.tables();
    let item: Option<i64> = sqlx::query_scalar(&format!(
        "SELECT id FROM {table} WHERE id=$1 AND repo_id=$2 FOR UPDATE"
    ))
    .bind(item_id)
    .bind(repo_id)
    .fetch_optional(&mut **tx)
    .await?;
    anyhow::ensure!(item.is_some(), LabelWriteRefusal::InvalidLabels);
    for id in &ids {
        let label: Option<i64> =
            sqlx::query_scalar("SELECT id FROM labels WHERE id=$1 AND repo_id=$2 FOR UPDATE")
                .bind(id)
                .bind(repo_id)
                .fetch_optional(&mut **tx)
                .await?;
        anyhow::ensure!(label.is_some(), LabelWriteRefusal::InvalidLabels);
    }
    Ok(ids)
}

pub async fn create_label(
    pool: &PgPool,
    actor_id: i64,
    repo_id: i64,
    name: &str,
    color: &str,
    description: Option<&str>,
) -> Result<Label> {
    let mut tx = begin_label_write(pool, actor_id, repo_id).await?;
    let label = sqlx::query_as::<_, Label>(
        r#"
        INSERT INTO labels (repo_id, name, color, description)
        VALUES ($1, $2, $3, $4)
        RETURNING *
        "#,
    )
    .bind(repo_id)
    .bind(name)
    .bind(color)
    .bind(description)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;

    Ok(label)
}

pub async fn list_repo_labels(pool: &PgPool, repo_id: i64) -> Result<Vec<Label>> {
    let labels =
        sqlx::query_as::<_, Label>("SELECT * FROM labels WHERE repo_id = $1 ORDER BY name")
            .bind(repo_id)
            .fetch_all(pool)
            .await?;

    Ok(labels)
}

/// Each label of the repository with the number of issues and pull requests it is on, as
/// `(label id, count)`. The association tables are keyed by item, not by label, so the counts
/// start from the repository's issues and pull requests, which are indexed by repository.
pub async fn list_label_usage(pool: &PgPool, repo_id: i64) -> Result<Vec<(i64, i64)>> {
    let usage = sqlx::query_as::<_, (i64, i64)>(
        r"SELECT l.id, COUNT(u.label_id)
        FROM labels l
        LEFT JOIN (
            SELECT il.label_id FROM issues i
            JOIN issue_labels il ON il.issue_id = i.id
            WHERE i.repo_id = $1
            UNION ALL
            SELECT pl.label_id FROM pull_requests p
            JOIN pr_labels pl ON pl.pull_request_id = p.id
            WHERE p.repo_id = $1
        ) u ON u.label_id = l.id
        WHERE l.repo_id = $1
        GROUP BY l.id",
    )
    .bind(repo_id)
    .fetch_all(pool)
    .await?;

    Ok(usage)
}

pub async fn update_label(
    pool: &PgPool,
    actor_id: i64,
    repo_id: i64,
    label_id: i64,
    name: Option<&str>,
    color: Option<&str>,
    description: Option<&str>,
) -> Result<Label> {
    let mut tx = begin_label_write(pool, actor_id, repo_id).await?;
    let label = sqlx::query_as::<_, Label>(
        r#"
        UPDATE labels SET
            name = COALESCE($3, name),
            color = COALESCE($4, color),
            description = COALESCE($5, description)
        WHERE id = $1 AND repo_id = $2
        RETURNING *
        "#,
    )
    .bind(label_id)
    .bind(repo_id)
    .bind(name)
    .bind(color)
    .bind(description)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(LabelWriteRefusal::InvalidLabels)?;
    tx.commit().await?;
    Ok(label)
}

pub async fn delete_label(
    pool: &PgPool,
    actor_id: i64,
    repo_id: i64,
    label_id: i64,
) -> Result<bool> {
    let mut tx = begin_label_write(pool, actor_id, repo_id).await?;
    let result = sqlx::query("DELETE FROM labels WHERE id = $1 AND repo_id = $2")
        .bind(label_id)
        .bind(repo_id)
        .execute(&mut *tx)
        .await?;
    anyhow::ensure!(
        result.rows_affected() == 1,
        LabelWriteRefusal::InvalidLabels
    );
    tx.commit().await?;
    Ok(true)
}

pub async fn add_issue_label(
    pool: &PgPool,
    actor_id: i64,
    repo_id: i64,
    issue_id: i64,
    label_id: i64,
) -> Result<()> {
    add_issue_labels(pool, actor_id, repo_id, issue_id, &[label_id]).await?;
    Ok(())
}

pub async fn add_issue_labels(
    pool: &PgPool,
    actor_id: i64,
    repo_id: i64,
    issue_id: i64,
    label_ids: &[i64],
) -> Result<Vec<Label>> {
    write_item_labels(
        pool,
        actor_id,
        repo_id,
        LabelTarget::Issue,
        issue_id,
        label_ids,
        false,
    )
    .await
}

pub async fn add_pull_request_labels(
    pool: &PgPool,
    actor_id: i64,
    repo_id: i64,
    pull_request_id: i64,
    label_ids: &[i64],
) -> Result<Vec<Label>> {
    write_item_labels(
        pool,
        actor_id,
        repo_id,
        LabelTarget::PullRequest,
        pull_request_id,
        label_ids,
        false,
    )
    .await
}

pub async fn remove_issue_label(
    pool: &PgPool,
    actor_id: i64,
    repo_id: i64,
    issue_id: i64,
    label_id: i64,
) -> Result<bool> {
    remove_item_label(
        pool,
        actor_id,
        repo_id,
        LabelTarget::Issue,
        issue_id,
        label_id,
    )
    .await
}

pub async fn remove_pull_request_label(
    pool: &PgPool,
    actor_id: i64,
    repo_id: i64,
    pull_request_id: i64,
    label_id: i64,
) -> Result<bool> {
    remove_item_label(
        pool,
        actor_id,
        repo_id,
        LabelTarget::PullRequest,
        pull_request_id,
        label_id,
    )
    .await
}

async fn remove_item_label(
    pool: &PgPool,
    actor_id: i64,
    repo_id: i64,
    target: LabelTarget,
    item_id: i64,
    label_id: i64,
) -> Result<bool> {
    let (_, link, column) = target.tables();
    let mut tx = begin_label_write(pool, actor_id, repo_id).await?;
    lock_item_labels(&mut tx, repo_id, target, item_id, &[label_id]).await?;
    let result = sqlx::query(&format!(
        "DELETE FROM {link} WHERE {column} = $1 AND label_id = $2"
    ))
    .bind(item_id)
    .bind(label_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(result.rows_affected() > 0)
}

pub async fn get_issue_labels(pool: &PgPool, issue_id: i64) -> Result<Vec<Label>> {
    get_item_labels(pool, LabelTarget::Issue, issue_id).await
}

pub async fn get_pull_request_labels(pool: &PgPool, pull_request_id: i64) -> Result<Vec<Label>> {
    get_item_labels(pool, LabelTarget::PullRequest, pull_request_id).await
}

async fn get_item_labels(pool: &PgPool, target: LabelTarget, item_id: i64) -> Result<Vec<Label>> {
    let (_, link, column) = target.tables();
    let labels = sqlx::query_as::<_, Label>(&format!(
        "SELECT l.* FROM labels l INNER JOIN {link} il ON l.id = il.label_id WHERE il.{column} = $1 ORDER BY l.name"
    ))
    .bind(item_id)
    .fetch_all(pool)
    .await?;

    Ok(labels)
}

pub async fn set_issue_labels(
    pool: &PgPool,
    actor_id: i64,
    repo_id: i64,
    issue_id: i64,
    label_ids: &[i64],
) -> Result<()> {
    write_item_labels(
        pool,
        actor_id,
        repo_id,
        LabelTarget::Issue,
        issue_id,
        label_ids,
        true,
    )
    .await?;
    Ok(())
}

async fn write_item_labels(
    pool: &PgPool,
    actor_id: i64,
    repo_id: i64,
    target: LabelTarget,
    item_id: i64,
    label_ids: &[i64],
    replace: bool,
) -> Result<Vec<Label>> {
    let (_, link, column) = target.tables();
    let mut tx = begin_label_write(pool, actor_id, repo_id).await?;
    // Validate the entire bounded, deduplicated batch before any effect.
    let ids = lock_item_labels(&mut tx, repo_id, target, item_id, label_ids).await?;
    if replace {
        sqlx::query(&format!("DELETE FROM {link} WHERE {column}=$1"))
            .bind(item_id)
            .execute(&mut *tx)
            .await?;
    }
    for id in ids {
        sqlx::query(&format!(
            "INSERT INTO {link}({column},label_id) VALUES ($1,$2) ON CONFLICT DO NOTHING"
        ))
        .bind(item_id)
        .bind(id)
        .execute(&mut *tx)
        .await?;
    }
    let labels = sqlx::query_as::<_, Label>(&format!(
        "SELECT l.* FROM labels l JOIN {link} il ON il.label_id=l.id WHERE il.{column}=$1 AND l.repo_id=$2 ORDER BY l.name"
    ))
    .bind(item_id)
    .bind(repo_id)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(labels)
}

pub(crate) async fn get_items_labels(
    pool: &PgPool,
    target: LabelTarget,
    item_ids: &[i64],
) -> Result<Vec<ItemLabel>> {
    debug_assert!(!item_ids.is_empty() && item_ids.len() <= 500);
    let (_, link, column) = target.tables();
    let mut query = sqlx::QueryBuilder::<sqlx::Postgres>::new(format!(
        "SELECT il.{column} AS item_id, l.* FROM {link} il JOIN labels l ON l.id=il.label_id WHERE il.{column} IN ("
    ));
    let mut ids = query.separated(",");
    for id in item_ids {
        ids.push_bind(id);
    }
    query.push(format!(") ORDER BY il.{column}, l.name"));
    Ok(query.build_query_as().fetch_all(pool).await?)
}
