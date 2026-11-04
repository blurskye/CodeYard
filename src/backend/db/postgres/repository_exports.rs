use crate::backend::db::organizations::OrganizationOutcome;
use crate::backend::db::repository_exports::ExportOptions;
use crate::backend::db::repository_exports::ExportRefusal;
use crate::backend::db::repository_exports::RepositoryExport;
use anyhow::Result;
use codeyard_db_core::models::OrgRole;
use sqlx::{PgConnection, PgPool};

async fn authority(connection: &mut PgConnection, actor: i64, org: Option<i64>) -> Result<bool> {
    if let Some(org) = org {
        return Ok(matches!(
            super::organizations::authority::authority(connection, actor, org).await?,
            OrganizationOutcome::Applied(codeyard_db_core::models::OrgRole::Owner)
        ));
    }
    super::users::transitions::lock_accounts_shared(connection, &[actor]).await?;
    Ok(sqlx::query_scalar::<_, i64>(
        "SELECT id FROM users WHERE id=$1 AND NOT is_banned AND NOT must_change_password FOR SHARE",
    )
    .bind(actor)
    .fetch_optional(connection)
    .await?
    .is_some())
}

async fn selected(
    connection: &mut PgConnection,
    actor: i64,
    org: Option<i64>,
    ids: &[i64],
    allow_deleted: bool,
) -> Result<bool> {
    for id in ids {
        let owned: Option<bool> = sqlx::query_scalar("SELECT COALESCE((($3::bigint IS NULL AND owner_kind='user' AND owner_user_id=$2) OR (owner_kind='org' AND owner_org_id=$3)),false) FROM repositories WHERE id=$1 FOR SHARE").bind(id).bind(actor).bind(org).fetch_optional(&mut *connection).await?;
        if owned != Some(true) && !(allow_deleted && owned.is_none()) {
            return Ok(false);
        }
    }
    Ok(true)
}

async fn scoped(
    connection: &mut PgConnection,
    actor: i64,
    org: Option<i64>,
    id: i64,
) -> Result<Option<RepositoryExport>> {
    if !authority(connection, actor, org).await? {
        return Ok(None);
    }
    let row: Option<RepositoryExport> = sqlx::query_as("SELECT * FROM repository_exports WHERE id=$1 AND (($3::bigint IS NULL AND owner_user_id=$2) OR owner_org_id=$3)").bind(id).bind(actor).bind(org).fetch_optional(&mut *connection).await?;
    Ok(row)
}

/// Export enumeration is pinned by a descending ID cursor, never a shifting offset.
pub async fn discussion_ids(
    pool: &PgPool,
    repo: i64,
    pulls: bool,
    before: i64,
    limit: i64,
) -> Result<Vec<i64>> {
    anyhow::ensure!(
        before > 0 && (1..=100).contains(&limit),
        ExportRefusal::Invalid
    );
    let table = if pulls { "pull_requests" } else { "issues" };
    Ok(sqlx::query_scalar(&format!(
        "SELECT id FROM {table} WHERE repo_id=$1 AND id<$2 ORDER BY id DESC LIMIT $3"
    ))
    .bind(repo)
    .bind(before)
    .bind(limit)
    .fetch_all(pool)
    .await?)
}

pub async fn create(
    pool: &PgPool,
    actor: i64,
    org: Option<i64>,
    ids: &[i64],
    options: &ExportOptions,
) -> Result<RepositoryExport> {
    let mut tx = pool.begin().await?;
    anyhow::ensure!(authority(&mut tx, actor, org).await?, ExportRefusal::Denied);
    anyhow::ensure!(
        ids.len() <= 100
            && ids.iter().collect::<std::collections::BTreeSet<_>>().len() == ids.len()
            && selected(&mut tx, actor, org, ids, false).await?
            && (!ids.is_empty() || options.org_metadata_only)
            && (!options.org_metadata_only || (org.is_some() && ids.is_empty())),
        ExportRefusal::Invalid
    );
    // Serialize admission for the same actor without holding any network work in this transaction.
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(0x4d49470000000000_i64.wrapping_add(actor))
        .execute(&mut *tx)
        .await?;
    let active: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM repository_exports WHERE actor_id=$1 AND state IN ('pending','exporting') AND updated_at>$2").bind(actor).bind(chrono::Utc::now().timestamp()-1200).fetch_one(&mut *tx).await?;
    anyhow::ensure!(active < 2, ExportRefusal::Busy);
    let now = chrono::Utc::now().timestamp();
    let row: RepositoryExport = sqlx::query_as("INSERT INTO repository_exports(guid,owner_user_id,owner_org_id,actor_id,repositories,options,state,created_at,updated_at) VALUES($1,$2,$3,$4,$5,$6,'pending',$7,$7) RETURNING *")
        .bind(uuid::Uuid::new_v4().to_string()).bind(if org.is_none() { Some(actor) } else { None }).bind(org).bind(actor).bind(serde_json::to_string(ids)?).bind(serde_json::to_string(options)?).bind(now).fetch_one(&mut *tx).await?;
    sqlx::query("INSERT INTO pending_object_deletions(object_key,delete_after) VALUES($1,$2)")
        .bind(row.object_key())
        .bind(chrono::Utc::now() + chrono::Duration::hours(1))
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(row)
}

pub async fn get(
    pool: &PgPool,
    actor: i64,
    org: Option<i64>,
    id: i64,
) -> Result<Option<RepositoryExport>> {
    let mut tx = pool.begin().await?;
    let row = scoped(&mut tx, actor, org, id).await?;
    tx.commit().await?;
    Ok(row)
}

pub async fn by_guid(pool: &PgPool, actor: i64, guid: &str) -> Result<Option<RepositoryExport>> {
    let mut tx = pool.begin().await?;
    let row: Option<RepositoryExport> =
        sqlx::query_as("SELECT * FROM repository_exports WHERE guid=$1")
            .bind(guid)
            .fetch_optional(&mut *tx)
            .await?;
    let row = if let Some(row) = row {
        scoped(&mut tx, actor, row.owner_org_id, row.id).await?
    } else {
        None
    };
    if let Some(row) = &row {
        if !selected(
            &mut tx,
            actor,
            row.owner_org_id,
            &row.repository_ids()?,
            row.state == "exported",
        )
        .await?
        {
            return Ok(None);
        }
    }
    tx.commit().await?;
    Ok(row)
}

pub async fn list(
    pool: &PgPool,
    actor: i64,
    org: Option<i64>,
    limit: i64,
    offset: i64,
) -> Result<Vec<RepositoryExport>> {
    let mut tx = pool.begin().await?;
    anyhow::ensure!(authority(&mut tx, actor, org).await?, ExportRefusal::Denied);
    let rows: Vec<RepositoryExport> = sqlx::query_as("SELECT * FROM repository_exports WHERE (($2::bigint IS NULL AND owner_user_id=$1) OR owner_org_id=$2) ORDER BY id DESC LIMIT $3 OFFSET $4").bind(actor).bind(org).bind(limit).bind(offset).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(rows)
}

pub async fn finish(pool: &PgPool, id: i64, state: &str) -> Result<()> {
    anyhow::ensure!(
        matches!(state, "exporting" | "exported" | "failed"),
        "Invalid migration state"
    );
    let mut tx = pool.begin().await?;
    let changed = sqlx::query("UPDATE repository_exports SET state=$2,updated_at=$3 WHERE id=$1 AND NOT archive_deleted AND state IN ('pending','exporting')").bind(id).bind(state).bind(chrono::Utc::now().timestamp()).execute(&mut *tx).await?.rows_affected();
    if state == "exported" && changed == 1 {
        sqlx::query("INSERT INTO pending_object_deletions(object_key,delete_after) SELECT 'migrations/' || guid,clock_timestamp()+INTERVAL '7 days' FROM repository_exports WHERE id=$1 ON CONFLICT(object_key) DO UPDATE SET delete_after=excluded.delete_after").bind(id).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(())
}

pub async fn delete_archive(
    pool: &PgPool,
    actor: i64,
    org: Option<i64>,
    id: i64,
) -> Result<Option<RepositoryExport>> {
    let mut tx = pool.begin().await?;
    let Some(row) = scoped(&mut tx, actor, org, id).await? else {
        return Ok(None);
    };
    // An export whose worker died twenty minutes ago reads as failed and can be deleted.
    let stale = matches!(row.state.as_str(), "pending" | "exporting")
        && row.updated_at + 1200 < chrono::Utc::now().timestamp();
    anyhow::ensure!(
        row.state == "exported" || row.state == "failed" || stale,
        ExportRefusal::Archive
    );
    sqlx::query("UPDATE repository_exports SET archive_deleted=TRUE,state=CASE WHEN state IN ('pending','exporting') THEN 'failed' ELSE state END,updated_at=$2 WHERE id=$1")
        .bind(id)
        .bind(chrono::Utc::now().timestamp())
        .execute(&mut *tx)
        .await?;
    sqlx::query("INSERT INTO pending_object_deletions(object_key,delete_after) VALUES($1,$2) ON CONFLICT(object_key) DO UPDATE SET delete_after=GREATEST(pending_object_deletions.delete_after,excluded.delete_after)").bind(row.object_key()).bind(chrono::Utc::now()).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(Some(row))
}

pub async fn lock_repository(pool: &PgPool, actor: i64, migration: i64, repo: i64) -> Result<()> {
    let mut tx = pool.begin().await?;
    let row: RepositoryExport = sqlx::query_as("SELECT * FROM repository_exports WHERE id=$1")
        .bind(migration)
        .fetch_one(&mut *tx)
        .await?;
    anyhow::ensure!(
        authority(&mut tx, actor, row.owner_org_id).await?
            && (row.owner_org_id.is_some() || row.owner_user_id == Some(actor)),
        ExportRefusal::Denied
    );
    anyhow::ensure!(
        row.actor_id == Some(actor)
            && matches!(row.state.as_str(), "pending" | "exporting")
            && row.repository_ids()?.contains(&repo),
        ExportRefusal::Invalid
    );
    let owned = sqlx::query_scalar::<_, i64>("SELECT id FROM repositories WHERE id=$1 AND (($3::bigint IS NULL AND owner_kind='user' AND owner_user_id=$2) OR (owner_kind='org' AND owner_org_id=$3)) FOR UPDATE").bind(repo).bind(actor).bind(row.owner_org_id).fetch_optional(&mut *tx).await?;
    anyhow::ensure!(owned.is_some(), ExportRefusal::Invalid);
    let held: Option<i64> =
        sqlx::query_scalar("SELECT migration_id FROM repository_migration_locks WHERE repo_id=$1")
            .bind(repo)
            .fetch_optional(&mut *tx)
            .await?;
    anyhow::ensure!(
        held.is_none() || held == Some(migration),
        ExportRefusal::Locked
    );
    sqlx::query("INSERT INTO repository_migration_locks(repo_id,migration_id) VALUES($1,$2) ON CONFLICT(repo_id) DO NOTHING").bind(repo).bind(migration).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

pub async fn unlock_repository(
    pool: &PgPool,
    actor: i64,
    org: Option<i64>,
    migration: i64,
    name: &str,
) -> Result<bool> {
    let mut tx = pool.begin().await?;
    let Some(row) = scoped(&mut tx, actor, org, migration).await? else {
        return Ok(false);
    };
    let ids = row.repository_ids()?;
    let repo: Option<i64> = sqlx::query_scalar("SELECT id FROM repositories WHERE id=ANY($4) AND LOWER(name)=LOWER($1) AND (($3::bigint IS NULL AND owner_user_id=$2) OR owner_org_id=$3) ORDER BY (name=$1) DESC,id LIMIT 1").bind(name).bind(actor).bind(org).bind(&ids).fetch_optional(&mut *tx).await?;
    let Some(repo) = repo else {
        return Ok(false);
    };
    sqlx::query("DELETE FROM repository_migration_locks WHERE repo_id=$1 AND migration_id=$2")
        .bind(repo)
        .bind(migration)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(true)
}

pub async fn locked(pool: &PgPool, repo: i64) -> Result<bool> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM repository_migration_locks WHERE repo_id=$1)",
    )
    .bind(repo)
    .fetch_one(pool)
    .await?)
}
