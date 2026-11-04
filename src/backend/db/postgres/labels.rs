use crate::backend::db::labels::bounded_label_ids;
use crate::backend::db::labels::ItemLabel;
use crate::backend::db::labels::LabelTarget;
use crate::backend::db::labels::LabelWriteRefusal;
use crate::backend::db::labels::PAGE_DESCRIPTION_CHARS;
use anyhow::Result;
use codeyard_db_core::models::Label;
use sqlx::{PgPool, Postgres, Transaction};

async fn begin_label_write<'a>(
    pool: &'a PgPool,
    actor: &crate::backend::db::credentials::Actor,
    repo_id: i64,
) -> Result<Transaction<'a, Postgres>> {
    let mut tx = pool.begin().await?;
    // CI singleton FIRST, before any issue/label lock. Retain authority to commit. Catalog
    // writers queue on the repository row, so a name check holds until the commit.
    anyhow::ensure!(
        crate::backend::db::postgres::discussions::lock_exclusive_write_authority(
            &mut tx,
            actor.user_id,
            repo_id
        )
        .await?,
        LabelWriteRefusal::Denied
    );
    anyhow::ensure!(
        writes_labels(&mut tx, actor, repo_id).await?,
        LabelWriteRefusal::Denied
    );
    Ok(tx)
}

/// Label names are unique in a repository regardless of case, as lookups by name are.
async fn ensure_name_free(
    tx: &mut Transaction<'_, Postgres>,
    repo_id: i64,
    name: &str,
    label_id: i64,
) -> Result<()> {
    let taken: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM labels WHERE repo_id=$1 AND lower(name)=lower($2) AND id<>$3)",
    )
    .bind(repo_id)
    .bind(name)
    .bind(label_id)
    .fetch_one(&mut **tx)
    .await?;
    anyhow::ensure!(!taken, LabelWriteRefusal::DuplicateName);
    Ok(())
}

/// Labels on an issue or pull request need triage authority; the label catalog needs write.
async fn begin_item_label_write<'a>(
    pool: &'a PgPool,
    actor: &crate::backend::db::credentials::Actor,
    repo_id: i64,
) -> Result<Transaction<'a, Postgres>> {
    let mut tx = pool.begin().await?;
    // CI singleton FIRST, before any issue/label lock. Retain authority to commit.
    anyhow::ensure!(
        crate::backend::db::postgres::discussions::lock_triage_authority(
            &mut tx,
            actor.user_id,
            repo_id
        )
        .await?,
        LabelWriteRefusal::Denied
    );
    anyhow::ensure!(
        writes_labels(&mut tx, actor, repo_id).await?,
        LabelWriteRefusal::Denied
    );
    Ok(tx)
}

/// The request's own session or token is still live and may still write issues or pull
/// requests here, checked in the transaction that writes.
async fn writes_labels(
    tx: &mut Transaction<'_, Postgres>,
    actor: &crate::backend::db::credentials::Actor,
    repo_id: i64,
) -> Result<bool> {
    Ok(
        crate::backend::db::postgres::credentials::repository(tx, actor, repo_id, "issues", true)
            .await?
            || crate::backend::db::postgres::credentials::repository(
                tx,
                actor,
                repo_id,
                "pull_requests",
                true,
            )
            .await?,
    )
}

pub(crate) async fn lock_item_labels(
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
    actor: impl Into<crate::backend::db::credentials::Actor>,
    repo_id: i64,
    name: &str,
    color: &str,
    description: Option<&str>,
) -> Result<Label> {
    let actor = actor.into();
    let mut tx = begin_label_write(pool, &actor, repo_id).await?;
    ensure_name_free(&mut tx, repo_id, name, 0).await?;
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
    .await
    .map_err(crate::backend::db::labels::label_database_error)?;
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

pub async fn get_repo_label_by_name(
    pool: &PgPool,
    repo_id: i64,
    name: &str,
) -> Result<Option<Label>> {
    Ok(
        sqlx::query_as("SELECT * FROM labels WHERE repo_id=$1 AND LOWER(name)=LOWER($2)")
            .bind(repo_id)
            .bind(name)
            .fetch_optional(pool)
            .await?,
    )
}

pub async fn get_label_by_id(pool: &PgPool, id: i64) -> Result<Option<Label>> {
    Ok(sqlx::query_as("SELECT * FROM labels WHERE id=$1")
        .bind(id)
        .fetch_optional(pool)
        .await?)
}

pub async fn count_repo_labels(pool: &PgPool, repo_id: i64, query: &str) -> Result<i64> {
    Ok(sqlx::query_scalar(r"SELECT COUNT(*) FROM labels l WHERE l.repo_id=$1 AND (l.name ILIKE $2::TEXT ESCAPE '\' OR COALESCE(l.description,'') ILIKE $2::TEXT ESCAPE '\')")
        .bind(repo_id).bind(crate::backend::db::discussions::contains_pattern(query.trim())).fetch_one(pool).await?)
}

pub async fn list_repo_labels_page(
    pool: &PgPool,
    repo_id: i64,
    query: &str,
    sort: &str,
    limit: i64,
    offset: i64,
) -> Result<Vec<Label>> {
    anyhow::ensure!(
        (1..=101).contains(&limit) && offset >= 0,
        "Invalid label page"
    );
    let order = match sort {
        "newest" => "l.id DESC",
        "oldest" => "l.id ASC",
        "name-desc" => "LOWER(l.name COLLATE \"C\") DESC, l.id DESC",
        "most-used" => "((SELECT COUNT(*) FROM issue_labels il WHERE il.label_id=l.id) + (SELECT COUNT(*) FROM pr_labels pl WHERE pl.label_id=l.id)) DESC, LOWER(l.name COLLATE \"C\"), l.id",
        _ => "LOWER(l.name COLLATE \"C\"), l.id",
    };
    Ok(sqlx::query_as(&format!(r#"SELECT l.id,l.repo_id,l.name,l.color,substr(l.description,1,{PAGE_DESCRIPTION_CHARS}) AS description,l.created_at,l.updated_at FROM labels l WHERE l.repo_id=$1 AND (l.name ILIKE $2::TEXT ESCAPE '\' OR COALESCE(l.description,'') ILIKE $2::TEXT ESCAPE '\') ORDER BY {order} LIMIT $3 OFFSET $4"#))
        .bind(repo_id).bind(crate::backend::db::discussions::contains_pattern(query.trim())).bind(limit).bind(offset).fetch_all(pool).await?)
}

/// Each label of the repository with the number of issues and pull requests it is on, as
/// `(label id, count)`. The association tables are keyed by item, not by label, so the counts
/// start from the repository's issues and pull requests, which are indexed by repository.
pub async fn list_milestone_labels(
    pool: &PgPool,
    viewer: Option<i64>,
    repo_id: i64,
    milestone_id: i64,
    limit: i64,
    offset: i64,
) -> Result<Vec<Label>> {
    Ok(sqlx::query_as(&format!("SELECT l.* FROM labels l WHERE l.repo_id=$1 AND EXISTS(SELECT 1 FROM repositories r LEFT JOIN users u ON u.id=$5 WHERE r.id=l.repo_id AND {}) AND (EXISTS (SELECT 1 FROM issue_labels il JOIN issues i ON i.id=il.issue_id WHERE il.label_id=l.id AND i.milestone_id=$2) OR EXISTS (SELECT 1 FROM pr_labels pl JOIN pull_requests p ON p.id=pl.pull_request_id WHERE pl.label_id=l.id AND p.milestone_id=$2)) ORDER BY lower(l.name),l.id LIMIT $3 OFFSET $4", crate::backend::db::postgres::repositories::access::readable_condition()))
        .bind(repo_id).bind(milestone_id).bind(limit).bind(offset).bind(viewer).fetch_all(pool).await?)
}

pub async fn list_label_usage(
    pool: &PgPool,
    repo_id: i64,
    label_ids: &[i64],
) -> Result<Vec<(i64, i64)>> {
    if label_ids.is_empty() {
        return Ok(Vec::new());
    }
    // Only the given labels are counted, through the label_id indexes.
    Ok(sqlx::query_as::<_, (i64, i64)>(
        r"SELECT l.id,
            (SELECT COUNT(*) FROM issue_labels il JOIN issues i ON i.id = il.issue_id
              WHERE il.label_id = l.id AND i.repo_id = $1)
          + (SELECT COUNT(*) FROM pr_labels pl JOIN pull_requests p ON p.id = pl.pull_request_id
              WHERE pl.label_id = l.id AND p.repo_id = $1)
        FROM labels l
        WHERE l.repo_id = $1 AND l.id = ANY($2)",
    )
    .bind(repo_id)
    .bind(label_ids)
    .fetch_all(pool)
    .await?)
}

pub async fn update_label(
    pool: &PgPool,
    actor: impl Into<crate::backend::db::credentials::Actor>,
    repo_id: i64,
    label_id: i64,
    name: Option<&str>,
    color: Option<&str>,
    description: Option<Option<&str>>,
) -> Result<Label> {
    update_label_checked(
        pool,
        actor,
        repo_id,
        label_id,
        (name, color, description),
        None,
    )
    .await
}

pub async fn update_label_checked(
    pool: &PgPool,
    actor: impl Into<crate::backend::db::credentials::Actor>,
    repo_id: i64,
    label_id: i64,
    (name, color, description): (Option<&str>, Option<&str>, Option<Option<&str>>),
    expected: Option<&str>,
) -> Result<Label> {
    let actor = actor.into();
    let mut tx = begin_label_write(pool, &actor, repo_id).await?;
    if let Some(expected) = expected {
        let current: Label =
            sqlx::query_as("SELECT * FROM labels WHERE repo_id=$1 AND id=$2 FOR UPDATE")
                .bind(repo_id)
                .bind(label_id)
                .fetch_optional(&mut *tx)
                .await?
                .ok_or(LabelWriteRefusal::InvalidLabels)?;
        let shown: String = current
            .description
            .unwrap_or_default()
            .chars()
            .take(PAGE_DESCRIPTION_CHARS)
            .collect();
        anyhow::ensure!(
            serde_json::to_string(&(current.name, current.color, shown))? == expected,
            LabelWriteRefusal::Conflict
        );
    }
    if let Some(name) = name {
        ensure_name_free(&mut tx, repo_id, name, label_id).await?;
    }
    let label = sqlx::query_as::<_, Label>(
        r#"
        UPDATE labels SET
            name = COALESCE($3, name),
            color = COALESCE($4, color),
            description = CASE WHEN $5 THEN $6 ELSE description END
        WHERE id = $1 AND repo_id = $2
        RETURNING *
        "#,
    )
    .bind(label_id)
    .bind(repo_id)
    .bind(name)
    .bind(color)
    .bind(description.is_some())
    .bind(description.flatten())
    .fetch_optional(&mut *tx)
    .await
    .map_err(crate::backend::db::labels::label_database_error)?
    .ok_or(LabelWriteRefusal::InvalidLabels)?;
    tx.commit().await?;
    Ok(label)
}

pub async fn delete_label(
    pool: &PgPool,
    actor: impl Into<crate::backend::db::credentials::Actor>,
    repo_id: i64,
    label_id: i64,
) -> Result<bool> {
    let actor = actor.into();
    let mut tx = begin_label_write(pool, &actor, repo_id).await?;
    // Deleting a label removes it from its issues and pull requests, so they get a new revision.
    for (table, links, column) in [
        ("issues", "issue_labels", "issue_id"),
        ("pull_requests", "pr_labels", "pull_request_id"),
    ] {
        sqlx::query(&format!("UPDATE {table} SET updated_at=GREATEST(clock_timestamp(),updated_at+INTERVAL '1 microsecond') WHERE id IN (SELECT x.{column} FROM {links} x JOIN labels l ON l.id=x.label_id WHERE l.id=$1 AND l.repo_id=$2)"))
            .bind(label_id)
            .bind(repo_id)
            .execute(&mut *tx)
            .await?;
    }
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
    actor: impl Into<crate::backend::db::credentials::Actor>,
    repo_id: i64,
    issue_id: i64,
    label_id: i64,
) -> Result<()> {
    add_issue_labels(pool, actor, repo_id, issue_id, &[label_id]).await?;
    Ok(())
}

pub async fn add_issue_labels(
    pool: &PgPool,
    actor: impl Into<crate::backend::db::credentials::Actor>,
    repo_id: i64,
    issue_id: i64,
    label_ids: &[i64],
) -> Result<Vec<Label>> {
    write_item_labels(
        pool,
        actor,
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
    actor: impl Into<crate::backend::db::credentials::Actor>,
    repo_id: i64,
    pull_request_id: i64,
    label_ids: &[i64],
) -> Result<Vec<Label>> {
    write_item_labels(
        pool,
        actor,
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
    actor: impl Into<crate::backend::db::credentials::Actor>,
    repo_id: i64,
    issue_id: i64,
    label_id: i64,
) -> Result<bool> {
    remove_item_label(pool, actor, repo_id, LabelTarget::Issue, issue_id, label_id).await
}

pub async fn remove_pull_request_label(
    pool: &PgPool,
    actor: impl Into<crate::backend::db::credentials::Actor>,
    repo_id: i64,
    pull_request_id: i64,
    label_id: i64,
) -> Result<bool> {
    remove_item_label(
        pool,
        actor,
        repo_id,
        LabelTarget::PullRequest,
        pull_request_id,
        label_id,
    )
    .await
}

async fn remove_item_label(
    pool: &PgPool,
    actor: impl Into<crate::backend::db::credentials::Actor>,
    repo_id: i64,
    target: LabelTarget,
    item_id: i64,
    label_id: i64,
) -> Result<bool> {
    let actor = actor.into();
    let (subject, link, column) = target.tables();
    let mut tx = begin_item_label_write(pool, &actor, repo_id).await?;
    lock_item_labels(&mut tx, repo_id, target, item_id, &[label_id]).await?;
    let before = item_labels_snapshot(&mut tx, target, item_id).await?;
    let result = sqlx::query(&format!(
        "DELETE FROM {link} WHERE {column} = $1 AND label_id = $2"
    ))
    .bind(item_id)
    .bind(label_id)
    .execute(&mut *tx)
    .await?;
    if result.rows_affected() > 0 {
        touch_subject(&mut tx, subject, item_id).await?;
        let after: Vec<_> = before
            .iter()
            .filter(|label| label.id != label_id)
            .cloned()
            .collect();
        record_label_changes(
            &mut tx,
            repo_id,
            target,
            item_id,
            actor.user_id,
            &before,
            &after,
        )
        .await?;
    }
    tx.commit().await?;
    Ok(result.rows_affected() > 0)
}

/// Moves an issue's or pull request's activity time forward when its labels or assignees
/// change, so sorting and incremental sync see the change.
pub(crate) async fn touch_subject(
    tx: &mut Transaction<'_, Postgres>,
    table: &str,
    id: i64,
) -> Result<()> {
    sqlx::query(&format!("UPDATE {table} SET updated_at=GREATEST(clock_timestamp(),updated_at+INTERVAL '1 microsecond') WHERE id=$1"))
        .bind(id)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

pub async fn get_issue_labels(pool: &PgPool, repo_id: i64, issue_id: i64) -> Result<Vec<Label>> {
    get_item_labels(pool, LabelTarget::Issue, repo_id, issue_id).await
}

pub async fn get_pull_request_labels(
    pool: &PgPool,
    repo_id: i64,
    pull_request_id: i64,
) -> Result<Vec<Label>> {
    get_item_labels(pool, LabelTarget::PullRequest, repo_id, pull_request_id).await
}

async fn get_item_labels(
    pool: &PgPool,
    target: LabelTarget,
    repo_id: i64,
    item_id: i64,
) -> Result<Vec<Label>> {
    let (_, link, column) = target.tables();
    let labels = sqlx::query_as::<_, Label>(&format!(
        "SELECT l.* FROM labels l INNER JOIN {link} il ON l.id = il.label_id WHERE il.{column} = $1 AND l.repo_id = $2 ORDER BY l.name"
    ))
    .bind(item_id)
    .bind(repo_id)
    .fetch_all(pool)
    .await?;

    Ok(labels)
}

pub async fn set_issue_labels(
    pool: &PgPool,
    actor: impl Into<crate::backend::db::credentials::Actor>,
    repo_id: i64,
    issue_id: i64,
    label_ids: &[i64],
) -> Result<()> {
    write_item_labels(
        pool,
        actor,
        repo_id,
        LabelTarget::Issue,
        issue_id,
        label_ids,
        true,
    )
    .await?;
    Ok(())
}

pub async fn set_pull_request_labels(
    pool: &PgPool,
    actor: impl Into<crate::backend::db::credentials::Actor>,
    repo_id: i64,
    pull_request_id: i64,
    label_ids: &[i64],
) -> Result<()> {
    write_item_labels(
        pool,
        actor,
        repo_id,
        LabelTarget::PullRequest,
        pull_request_id,
        label_ids,
        true,
    )
    .await?;
    Ok(())
}

async fn write_item_labels(
    pool: &PgPool,
    actor: impl Into<crate::backend::db::credentials::Actor>,
    repo_id: i64,
    target: LabelTarget,
    item_id: i64,
    label_ids: &[i64],
    replace: bool,
) -> Result<Vec<Label>> {
    let actor = actor.into();
    let mut tx = begin_item_label_write(pool, &actor, repo_id).await?;
    let labels = write_item_labels_transaction(
        &mut tx,
        Some(actor.user_id),
        repo_id,
        target,
        item_id,
        label_ids,
        replace,
    )
    .await?;
    tx.commit().await?;
    Ok(labels)
}

/// Caller retains live item-metadata authority until this transaction commits.
pub(crate) async fn write_item_labels_transaction(
    tx: &mut Transaction<'_, Postgres>,
    actor: Option<i64>,
    repo_id: i64,
    target: LabelTarget,
    item_id: i64,
    label_ids: &[i64],
    replace: bool,
) -> Result<Vec<Label>> {
    let (subject, link, column) = target.tables();
    let ids = lock_item_labels(tx, repo_id, target, item_id, label_ids).await?;
    let before = item_labels_snapshot(tx, target, item_id).await?;
    let mut changed = false;
    if replace {
        changed |= sqlx::query(&format!("DELETE FROM {link} WHERE {column}=$1"))
            .bind(item_id)
            .execute(&mut **tx)
            .await?
            .rows_affected()
            > 0;
    }
    for id in ids {
        changed |= sqlx::query(&format!(
            "INSERT INTO {link}({column},label_id) VALUES ($1,$2) ON CONFLICT DO NOTHING"
        ))
        .bind(item_id)
        .bind(id)
        .execute(&mut **tx)
        .await?
        .rows_affected()
            > 0;
    }
    if changed {
        touch_subject(tx, subject, item_id).await?;
    }
    let labels = sqlx::query_as::<_, Label>(&format!(
        "SELECT l.* FROM labels l JOIN {link} il ON il.label_id=l.id WHERE il.{column}=$1 AND l.repo_id=$2 ORDER BY l.name"
    ))
    .bind(item_id)
    .bind(repo_id)
    .fetch_all(&mut **tx)
    .await?;
    anyhow::ensure!(
        labels.len() <= crate::backend::db::labels::MAX_ISSUE_LABELS,
        LabelWriteRefusal::InvalidLabels
    );
    if let Some(actor) = actor {
        record_label_changes(tx, repo_id, target, item_id, actor, &before, &labels).await?;
    }
    Ok(labels)
}

async fn item_labels_snapshot(
    tx: &mut Transaction<'_, Postgres>,
    target: LabelTarget,
    item: i64,
) -> Result<Vec<Label>> {
    let (_, link, column) = target.tables();
    Ok(sqlx::query_as(&format!("SELECT l.* FROM labels l JOIN {link} il ON il.label_id=l.id WHERE il.{column}=$1 ORDER BY l.id"))
        .bind(item).fetch_all(&mut **tx).await?)
}

async fn record_label_changes(
    tx: &mut Transaction<'_, Postgres>,
    repo: i64,
    target: LabelTarget,
    item: i64,
    actor: i64,
    before: &[Label],
    after: &[Label],
) -> Result<()> {
    let subject = match target {
        LabelTarget::Issue => "issue",
        LabelTarget::PullRequest => "pull_request",
    };
    for (kind, old, new) in [("unlabeled", before, after), ("labeled", after, before)] {
        for label in old
            .iter()
            .filter(|label| !new.iter().any(|other| other.id == label.id))
        {
            crate::backend::db::postgres::timeline::append(
                tx,
                repo,
                subject,
                item,
                actor,
                kind,
                &label.name,
                &label.color,
            )
            .await?;
        }
    }
    Ok(())
}

/// On a connection, so a list can read its labels in the snapshot it read its rows in.
pub(crate) async fn get_items_labels(
    connection: &mut sqlx::PgConnection,
    target: LabelTarget,
    repo: Option<i64>,
    item_ids: &[i64],
) -> Result<Vec<ItemLabel>> {
    debug_assert!(!item_ids.is_empty() && item_ids.len() <= 500);
    let (_, link, column) = target.tables();
    let mut query = sqlx::QueryBuilder::<sqlx::Postgres>::new(format!(
        "SELECT il.{column} AS item_id, l.* FROM {link} il JOIN labels l ON l.id=il.label_id WHERE "
    ));
    // A repository's list reads only that repository's labels; `None` is for reads that
    // already chose their items in the same snapshot.
    if let Some(repo) = repo {
        query.push("l.repo_id=").push_bind(repo).push(" AND ");
    }
    query.push(format!("il.{column} IN ("));
    let mut ids = query.separated(",");
    for id in item_ids {
        ids.push_bind(id);
    }
    query.push(format!(") ORDER BY il.{column}, l.name"));
    Ok(query.build_query_as().fetch_all(connection).await?)
}
