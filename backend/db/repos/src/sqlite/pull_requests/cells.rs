use crate::pull_requests::cells::Refresh;
use anyhow::{ensure, Result};
use codeyard_database::{
    cells::pulls::{Admission, Binding, Event},
    models::PullRequest,
    pool::SqlitePool,
    queries::credentials::Actor,
};

async fn readable(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    actor: &Actor,
    head: i64,
) -> Result<bool> {
    Ok(actor.credential.is_some() && codeyard_database::queries::sqlite::repositories::access::lock_readable_repository_in_transaction(
        tx, actor.user_id, head,
    ).await? && codeyard_database::queries::sqlite::credentials::repository(tx, actor, head, "contents", false).await?)
}
pub async fn subscribe(
    pool: &SqlitePool,
    binding: &Binding,
    actor: &Actor,
    admission: &dyn Admission,
) -> Result<()> {
    binding.validate()?;
    let source = admission
        .check(binding.base(), binding.head(), None)
        .await?;
    // The base answers before any local lock is held; the writes below recheck the reader.
    let grant = pool
        .foreign_authorization()
        .ok_or_else(|| anyhow::anyhow!("Base cell is unavailable"))?
        .pull_grant(binding, None)
        .await?
        .ok_or(codeyard_types::credentials::AccountRestricted)?;
    grant.validate(binding)?;
    ensure!(
        grant.publisher_id == actor.user_id,
        codeyard_types::credentials::AccountRestricted
    );
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    ensure!(
        readable(&mut tx, actor, binding.head_repo_id).await?,
        codeyard_types::credentials::AccountRestricted
    );
    let head: Option<i64> = sqlx::query_scalar("SELECT id FROM repositories WHERE id=$1 AND NOT is_stub AND NOT is_gist AND home_cell=$2 AND forked_from_id=$3")
        .bind(binding.head_repo_id).bind(i32::from(pool.cell_id().get())).bind(binding.base_repo_id).fetch_optional(&mut *tx).await?;
    ensure!(head.is_some(), "Pull head has another home or source");
    ensure!(
        readable(&mut tx, actor, binding.head_repo_id).await?,
        codeyard_types::credentials::AccountRestricted
    );
    sqlx::query("INSERT INTO remote_pull_requests(base_repo_id,number,head_repo_id,head_branch,publisher_id) VALUES($1,$2,$3,$4,$5) ON CONFLICT(base_repo_id,number) DO UPDATE SET head_repo_id=excluded.head_repo_id,head_branch=excluded.head_branch,publisher_id=excluded.publisher_id")
        .bind(binding.base_repo_id).bind(binding.number).bind(binding.head_repo_id).bind(&binding.head_branch).bind(actor.user_id).execute(&mut *tx).await?;
    ensure!(
        admission
            .check(binding.base(), binding.head(), None)
            .await?
            == source,
        "Pull source changed"
    );
    ensure!(
        readable(&mut tx, actor, binding.head_repo_id).await?,
        codeyard_types::credentials::AccountRestricted
    );
    tx.commit().await?;
    Ok(())
}
pub async fn receive_push(
    pool: &SqlitePool,
    event: &Event,
    admission: &dyn Admission,
) -> Result<()> {
    event.validate()?;
    let source = admission
        .check(event.change.head(), event.change.base(), Some(event.id))
        .await?;
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let base: Option<i64> = sqlx::query_scalar(
        "SELECT id FROM repositories WHERE id=$1 AND NOT is_stub AND NOT is_gist AND home_cell=$2",
    )
    .bind(event.change.base_repo_id)
    .bind(i32::from(pool.cell_id().get()))
    .fetch_optional(&mut *tx)
    .await?;
    let interested: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pull_requests WHERE repo_id=$1 AND head_repo_id=$2 AND state='open')")
        .bind(event.change.base_repo_id).bind(event.change.head_repo_id).fetch_one(&mut *tx).await?;
    if base.is_none() || !interested {
        return Ok(());
    }
    let work = Refresh {
        push: event.change.clone(),
        after: 0,
    };
    work.validate()?;
    sqlx::query("INSERT INTO cell_outbox(root_kind,root_id,topic,payload,source_cell,source_event) VALUES('repository',$1,'pull.refresh',$2,$3,$4) ON CONFLICT(source_cell,source_event,topic) DO NOTHING")
        .bind(event.change.base_repo_id).bind(serde_json::to_string(&work)?).bind(i32::from(source.get())).bind(event.id).execute(&mut *tx).await?;
    ensure!(
        admission
            .check(event.change.head(), event.change.base(), Some(event.id))
            .await?
            == source,
        "Push source changed"
    );
    tx.commit().await?;
    Ok(())
}
/// Runs after the branch moved, so the base never fetches a head older than the event.
pub async fn queue_push(pool: &SqlitePool, head: i64) -> Result<bool> {
    codeyard_types::cell_ids::Prefix::for_id(head)?;
    let queued = sqlx::query("INSERT INTO cell_outbox(root_kind,root_id,topic,payload) SELECT 'repository',r.id,'repository.push',json_object('base_repo_id',r.forked_from_id,'head_repo_id',r.id,'version',r.contribution_version) FROM repositories r JOIN repositories p ON p.id=r.forked_from_id AND p.is_stub AND p.home_cell<>r.home_cell WHERE r.id=$1 AND NOT r.is_stub AND NOT r.is_gist AND r.home_cell=COALESCE((SELECT cell_id FROM node_configuration WHERE id=1),0) AND EXISTS(SELECT 1 FROM remote_pull_requests c WHERE c.head_repo_id=r.id AND c.base_repo_id=r.forked_from_id)")
        .bind(head).execute(pool).await?;
    Ok(queued.rows_affected() > 0)
}
pub async fn publisher(pool: &SqlitePool, binding: &Binding) -> Result<Option<i64>> {
    binding.validate()?;
    Ok(sqlx::query_scalar("SELECT publisher_id FROM remote_pull_requests WHERE base_repo_id=$1 AND number=$2 AND head_repo_id=$3 AND head_branch=$4")
        .bind(binding.base_repo_id).bind(binding.number).bind(binding.head_repo_id).bind(&binding.head_branch).fetch_optional(pool).await?)
}
pub async fn open(pool: &SqlitePool, refresh: &Refresh, limit: i64) -> Result<Vec<PullRequest>> {
    refresh.validate()?;
    ensure!(
        (1..=100).contains(&limit),
        "Pull refresh batch exceeds its limit"
    );
    Ok(sqlx::query_as("SELECT * FROM pull_requests WHERE repo_id=$1 AND head_repo_id=$2 AND state='open' AND id>$3 ORDER BY id LIMIT $4")
        .bind(refresh.push.base_repo_id).bind(refresh.push.head_repo_id).bind(refresh.after).bind(limit).fetch_all(pool).await?)
}
pub async fn bindings(
    pool: &SqlitePool,
    head: i64,
    after: i32,
    limit: i64,
) -> Result<Vec<Binding>> {
    codeyard_types::cell_ids::Prefix::for_id(head)?;
    ensure!(
        after >= 0 && (1..=100).contains(&limit),
        "Pull permission batch exceeds its limit"
    );
    let rows: Vec<(i64,i32,i64,String)> = sqlx::query_as("SELECT c.base_repo_id,c.number,c.head_repo_id,c.head_branch FROM remote_pull_requests c JOIN repositories r ON r.id=c.head_repo_id AND r.forked_from_id=c.base_repo_id WHERE c.head_repo_id=$1 AND c.number>$3 ORDER BY c.number LIMIT $2")
        .bind(head).bind(limit).bind(after).fetch_all(pool).await?;
    rows.into_iter()
        .map(|(base_repo_id, number, head_repo_id, head_branch)| {
            let binding = Binding {
                base_repo_id,
                number,
                head_repo_id,
                head_branch,
            };
            binding.validate()?;
            Ok(binding)
        })
        .collect()
}
