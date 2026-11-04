use crate::backend::db::action_objects::ActionObject;
use crate::backend::db::action_objects::BlobPart;
use crate::backend::db::action_objects::NewObject;
use crate::backend::db::action_objects::RuntimeRun;
use anyhow::Result;
use sqlx::SqlitePool;

pub async fn issue_token(
    pool: &SqlitePool,
    run_id: i64,
    worker: &str,
    hash: &str,
    backend_id: &str,
) -> Result<()> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let live: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM ci_runs WHERE id=?1 AND worker=?2 AND status='running' AND NOT cancel_requested)")
        .bind(run_id).bind(worker).fetch_one(&mut *tx).await?;
    anyhow::ensure!(live, "run is not active");
    sqlx::query("INSERT INTO actions_runtime_tokens(run_id,token_hash,backend_id) VALUES(?1,?2,?3) ON CONFLICT(run_id) DO UPDATE SET token_hash=excluded.token_hash,backend_id=excluded.backend_id")
        .bind(run_id).bind(hash).bind(backend_id).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

// Default and base branches use the scope that a push of that branch stores (ci::push::branch_name).
pub async fn authenticate(pool: &SqlitePool, hash: &str) -> Result<Option<RuntimeRun>> {
    Ok(sqlx::query_as("SELECT r.id,r.repo_id,t.backend_id,CASE WHEN r.event='pull_request' THEN 'refs/pull/' || COALESCE(CAST(pr.number AS TEXT),CAST(r.id AS TEXT)) || '/merge' ELSE r.branch END AS branch,CASE WHEN r.event='pull_request' THEN (SELECT CASE WHEN substr(b.name,1,5)='refs/' THEN 'refs/heads/' || b.name ELSE b.name END FROM (SELECT COALESCE(json_extract(r.pull_request_event,'$.base_ref'),pr.base_branch) AS name) b) END AS base_branch,CASE WHEN substr(p.default_branch,1,5)='refs/' THEN 'refs/heads/' || p.default_branch ELSE p.default_branch END AS default_branch,r.is_fork FROM actions_runtime_tokens t JOIN ci_runs r ON r.id=t.run_id JOIN repositories p ON p.id=r.repo_id LEFT JOIN pull_requests pr ON pr.id=r.pull_request_id WHERE t.token_hash=$1 AND NOT r.cancel_requested AND (r.status='running' OR r.completed_at > strftime('%Y-%m-%dT%H:%M:%f000Z','now','-5 minutes'))")
        .bind(hash).fetch_optional(pool).await?)
}

pub async fn create_object(pool: &SqlitePool, new: NewObject<'_>) -> Result<Option<ActionObject>> {
    // Fork writes belong only to their derived pull-request merge-ref scope.
    if new.kind == "cache"
        && new.run.is_fork
        && !(new.run.branch.starts_with("refs/pull/") && new.run.branch.ends_with("/merge"))
    {
        return Ok(None);
    }
    let now = chrono::Utc::now().timestamp();
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let expires_at = if new.kind == "cache" {
        new.expires_at
            .min(now + i64::from(cache_policy(&mut tx, new.run.repo_id).await?.1) * 86400)
    } else {
        new.expires_at
    };
    // An expired key can be reserved again without waiting for the background sweep.
    sqlx::query("DELETE FROM actions_objects WHERE repo_id=$1 AND kind=$2 AND name=$3 AND version=$4 AND (run_id=$5 OR (kind='cache' AND scope=$6)) AND (expires_at<=$7 OR (NOT finalized AND EXISTS(SELECT 1 FROM ci_runs c WHERE c.id=actions_objects.run_id AND c.status NOT IN ('queued','running') AND c.completed_at<strftime('%Y-%m-%dT%H:%M:%f000Z','now','-5 minutes'))))")
        .bind(new.run.repo_id).bind(new.kind).bind(new.name).bind(new.version).bind(new.run.id).bind(&new.run.branch).bind(now).execute(&mut *tx).await?;
    let object = sqlx::query_as("INSERT INTO actions_objects(repo_id,run_id,kind,scope,name,version,created_at,accessed_at,expires_at) VALUES($1,$2,$3,$4,$5,$6,$7,$7,$8) ON CONFLICT DO NOTHING RETURNING *")
        .bind(new.run.repo_id).bind(new.run.id).bind(new.kind).bind(&new.run.branch)
        .bind(new.name).bind(new.version).bind(now).bind(expires_at).fetch_optional(&mut *tx).await?;
    // A lost create response leaves this run's own unfinished reservation. A retry gets it back, and
    // the caller issues it a fresh upload URL.
    let object: Option<ActionObject> = match object {
        Some(object) => Some(object),
        None => sqlx::query_as("SELECT * FROM actions_objects WHERE repo_id=$1 AND kind=$2 AND name=$3 AND version=$4 AND run_id=$5 AND NOT finalized")
            .bind(new.run.repo_id).bind(new.kind).bind(new.name).bind(new.version).bind(new.run.id)
            .fetch_optional(&mut *tx).await?,
    };
    tx.commit().await?;
    Ok(object)
}

pub async fn own_object(
    pool: &SqlitePool,
    run_id: i64,
    kind: &str,
    name: &str,
    version: &str,
) -> Result<Option<ActionObject>> {
    Ok(sqlx::query_as("SELECT * FROM actions_objects WHERE run_id=$1 AND kind=$2 AND name=$3 AND version=$4 AND expires_at>$5")
        .bind(run_id).bind(kind).bind(name).bind(version).bind(chrono::Utc::now().timestamp()).fetch_optional(pool).await?)
}

pub async fn grant_blob(
    pool: &SqlitePool,
    object_id: i64,
    run_id: i64,
    hash: &str,
    write: bool,
) -> Result<bool> {
    // Only while the run keeps its runtime token: losing access deletes the token and the
    // grants together, so a request that authenticated before that cannot grant afterwards.
    let granted = sqlx::query("INSERT INTO actions_blob_grants(token_hash,object_id,run_id,write,expires_at) SELECT $1,$2,$3,$4,$5 WHERE EXISTS(SELECT 1 FROM actions_runtime_tokens WHERE run_id=$3)")
        .bind(hash).bind(object_id).bind(run_id).bind(write).bind(chrono::Utc::now().timestamp()+3600).execute(pool).await?;
    Ok(granted.rows_affected() == 1)
}

pub async fn blob_grant(
    pool: &SqlitePool,
    object_id: i64,
    hash: &str,
    write: bool,
) -> Result<Option<ActionObject>> {
    Ok(sqlx::query_as("SELECT o.* FROM actions_blob_grants g JOIN actions_objects o ON o.id=g.object_id JOIN ci_runs r ON r.id=g.run_id WHERE o.id=$1 AND g.token_hash=$2 AND g.write=$3 AND g.expires_at>$4 AND o.expires_at>$4 AND (NOT $3 OR (r.status='running' OR r.completed_at > strftime('%Y-%m-%dT%H:%M:%f000Z','now','-5 minutes')))")
        .bind(object_id).bind(hash).bind(write).bind(chrono::Utc::now().timestamp()).fetch_optional(pool).await?)
}

async fn lock_blob_grant(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    object: i64,
    hash: &str,
) -> Result<bool> {
    Ok(sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM actions_blob_grants g JOIN ci_runs r ON r.id=g.run_id WHERE g.object_id=$1 AND g.token_hash=$2 AND g.write AND g.expires_at>$3 AND NOT r.cancel_requested AND (r.status='running' OR r.completed_at>strftime('%Y-%m-%dT%H:%M:%f000Z','now','-5 minutes')))")
        .bind(object).bind(hash).bind(chrono::Utc::now().timestamp()).fetch_one(&mut **tx).await?)
}

pub async fn stage_block(
    pool: &SqlitePool,
    object_id: i64,
    grant_hash: &str,
    part: &BlobPart,
    limit: i64,
) -> Result<bool> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    if !lock_blob_grant(&mut tx, object_id, grant_hash).await? {
        return Ok(false);
    }
    let object: Option<ActionObject> = sqlx::query_as("SELECT * FROM actions_objects WHERE id=$1")
        .bind(object_id)
        .fetch_optional(&mut *tx)
        .await?;
    let Some(object) = object.filter(|o| o.manifest.is_none() && !o.finalized) else {
        return Ok(false);
    };
    let limit = if object.kind == "cache" {
        limit.min(cache_policy(&mut tx, object.repo_id).await?.0)
    } else {
        limit
    };
    let total: i64 = sqlx::query_scalar(
        "SELECT COALESCE(SUM(size),0) FROM actions_blob_parts WHERE object_id=$1 AND block_id<>$2",
    )
    .bind(object.id)
    .bind(&part.block_id)
    .fetch_one(&mut *tx)
    .await?;
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM actions_blob_parts WHERE object_id=$1 AND block_id<>$2",
    )
    .bind(object.id)
    .bind(&part.block_id)
    .fetch_one(&mut *tx)
    .await?;
    if total > limit - part.size || count >= 50000 {
        return Ok(false);
    }
    if object.kind == "cache" {
        // Unfinished uploads count against the repository quota too. Finished caches make room by eviction.
        let pending: i64 = sqlx::query_scalar(
            "SELECT COALESCE(SUM(p.size),0) FROM actions_blob_parts p JOIN actions_objects o ON o.id=p.object_id WHERE o.repo_id=$1 AND o.kind='cache' AND NOT o.finalized AND NOT (p.object_id=$2 AND p.block_id=$3)",
        )
        .bind(object.repo_id)
        .bind(object.id)
        .bind(&part.block_id)
        .fetch_one(&mut *tx)
        .await?;
        let room = limit - pending - part.size;
        if room < 0 {
            return Ok(false);
        }
        enforce_cache_limit(&mut tx, object.repo_id, room).await?;
    }
    // Delete, rather than update, so the deletion trigger queues the replaced physical object.
    sqlx::query("DELETE FROM actions_blob_parts WHERE object_id=$1 AND block_id=$2")
        .bind(object.id)
        .bind(&part.block_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "INSERT INTO actions_blob_parts(object_id,block_id,object_key,size) VALUES($1,$2,$3,$4)",
    )
    .bind(object.id)
    .bind(&part.block_id)
    .bind(&part.object_key)
    .bind(part.size)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(true)
}

pub async fn ready_block(
    pool: &SqlitePool,
    object_id: i64,
    grant_hash: &str,
    key: &str,
) -> Result<bool> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    if !lock_blob_grant(&mut tx, object_id, grant_hash).await? {
        return Ok(false);
    }
    let changed = sqlx::query(
        "UPDATE actions_blob_parts SET ready=TRUE WHERE object_id=$1 AND object_key=$2",
    )
    .bind(object_id)
    .bind(key)
    .execute(&mut *tx)
    .await?
    .rows_affected()
        == 1;
    tx.commit().await?;
    Ok(changed)
}

/// Clear the block list of an upload that is not finalized, so a whole-blob PUT can stage it again.
pub async fn reopen_blob(pool: &SqlitePool, object_id: i64, grant_hash: &str) -> Result<()> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    anyhow::ensure!(
        lock_blob_grant(&mut tx, object_id, grant_hash).await?,
        "Blob capability refused"
    );
    sqlx::query("UPDATE actions_objects SET manifest=NULL WHERE id=$1 AND NOT finalized")
        .bind(object_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn seal_blob(
    pool: &SqlitePool,
    object_id: i64,
    grant_hash: &str,
    blocks: &[String],
) -> Result<bool> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    if !lock_blob_grant(&mut tx, object_id, grant_hash).await? {
        return Ok(false);
    }
    let object: Option<ActionObject> = sqlx::query_as("SELECT * FROM actions_objects WHERE id=$1")
        .bind(object_id)
        .fetch_optional(&mut *tx)
        .await?;
    let Some(object) = object else {
        return Ok(false);
    };
    if let Some(manifest) = object.manifest {
        let parts: Vec<BlobPart> = serde_json::from_str(&manifest)?;
        return Ok(parts.iter().map(|p| &p.block_id).eq(blocks.iter()));
    }
    let available: Vec<BlobPart> = sqlx::query_as(
        "SELECT block_id,object_key,size FROM actions_blob_parts WHERE object_id=$1 AND ready",
    )
    .bind(object_id)
    .fetch_all(&mut *tx)
    .await?;
    // Index the available parts once, so sealing a long block list stays linear.
    let by_id: std::collections::HashMap<&str, &BlobPart> = available
        .iter()
        .map(|part| (part.block_id.as_str(), part))
        .collect();
    let mut parts = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for id in blocks {
        let Some(part) = by_id.get(id.as_str()) else {
            return Ok(false);
        };
        if !seen.insert(id) {
            return Ok(false);
        }
        parts.push((*part).clone());
    }
    sqlx::query("DELETE FROM actions_blob_parts WHERE object_id=$1 AND NOT ready")
        .bind(object_id)
        .execute(&mut *tx)
        .await?;
    for part in &available {
        if !seen.contains(&part.block_id) {
            sqlx::query("DELETE FROM actions_blob_parts WHERE object_id=$1 AND block_id=$2")
                .bind(object_id)
                .bind(&part.block_id)
                .execute(&mut *tx)
                .await?;
        }
    }
    let size: i64 = parts.iter().map(|p| p.size).sum();
    sqlx::query("UPDATE actions_objects SET manifest=?2,size=?3 WHERE id=?1")
        .bind(object_id)
        .bind(serde_json::to_string(&parts)?)
        .bind(size)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(true)
}

pub async fn finalize_object(
    pool: &SqlitePool,
    run_id: i64,
    hash: &str,
    object_id: i64,
    size: i64,
    cache_limit: i64,
) -> Result<bool> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    // Serialize the repository's finalizations so concurrent uploads cannot exceed its cache quota.
    let repo: Option<i64> =
        sqlx::query_scalar("SELECT repo_id FROM actions_objects WHERE id=$1 AND run_id=$2")
            .bind(object_id)
            .bind(run_id)
            .fetch_optional(&mut *tx)
            .await?;
    let Some(repo) = repo else { return Ok(false) };
    sqlx::query("SELECT id FROM repositories WHERE id=$1")
        .bind(repo)
        .execute(&mut *tx)
        .await?;
    let live: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM actions_runtime_tokens t JOIN ci_runs r ON r.id=t.run_id WHERE t.run_id=$1 AND t.token_hash=$2 AND NOT r.cancel_requested AND (r.status='running' OR r.completed_at>strftime('%Y-%m-%dT%H:%M:%f000Z','now','-5 minutes')))")
        .bind(run_id).bind(hash).fetch_one(&mut *tx).await?;
    if !live {
        return Ok(false);
    }
    let object: ActionObject = sqlx::query_as("SELECT * FROM actions_objects WHERE id=$1")
        .bind(object_id)
        .fetch_one(&mut *tx)
        .await?;
    let cache_limit = cache_limit.min(cache_policy(&mut tx, repo).await?.0);
    if object.manifest.is_none()
        || object.size != size
        || (object.kind == "cache" && size > cache_limit)
    {
        return Ok(false);
    }
    sqlx::query("UPDATE actions_objects SET finalized=TRUE WHERE id=$1")
        .bind(object_id)
        .execute(&mut *tx)
        .await?;
    enforce_cache_limit(&mut tx, repo, cache_limit).await?;
    tx.commit().await?;
    Ok(true)
}

pub(crate) async fn cache_policy(
    connection: &mut sqlx::SqliteConnection,
    repo: i64,
) -> Result<(i64, i32)> {
    let (site_size,site_days,repo_size,repo_days,org_size,org_days): crate::backend::db::action_objects::CacheSettings = sqlx::query_as("SELECT (SELECT cache_size_gib FROM actions_settings WHERE id=1),(SELECT cache_retention_days FROM actions_settings WHERE id=1),(SELECT cache_size_gib FROM repo_actions_settings WHERE repo_id=$1),(SELECT cache_retention_days FROM repo_actions_settings WHERE repo_id=$1),(SELECT s.cache_size_gib FROM org_actions_settings s JOIN repositories r ON r.owner_org_id=s.org_id AND r.owner_kind='org' WHERE r.id=$1),(SELECT s.cache_retention_days FROM org_actions_settings s JOIN repositories r ON r.owner_org_id=s.org_id AND r.owner_kind='org' WHERE r.id=$1)").bind(repo).fetch_one(connection).await?;
    use crate::backend::db::actions::CacheLimit;
    Ok((
        i64::from(CacheLimit::Storage.effective(site_size, org_size, repo_size))
            * 1024
            * 1024
            * 1024,
        CacheLimit::Retention.effective(site_days, org_days, repo_days),
    ))
}

/// Apply the repository's current cache size and retention limits to its existing caches.
pub(crate) async fn apply_cache_policy(
    connection: &mut sqlx::SqliteConnection,
    repo: i64,
) -> Result<()> {
    let (bytes, days) = cache_policy(&mut *connection, repo).await?;
    enforce_cache_limit(&mut *connection, repo, bytes).await?;
    sqlx::query("UPDATE actions_objects SET expires_at=MIN(expires_at,accessed_at+$2) WHERE repo_id=$1 AND kind='cache'")
        .bind(repo)
        .bind(i64::from(days) * 86400)
        .execute(&mut *connection)
        .await?;
    Ok(())
}

pub(crate) async fn enforce_cache_limit(
    connection: &mut sqlx::SqliteConnection,
    repo: i64,
    cache_limit: i64,
) -> Result<()> {
    let (mut total, mut count): (i64, i64) = sqlx::query_as("SELECT CAST(COALESCE(SUM(size),0) AS BIGINT),COUNT(*) FROM actions_objects WHERE repo_id=$1 AND kind='cache' AND finalized")
        .bind(repo).fetch_one(&mut *connection).await?;
    while total > cache_limit
        || count > crate::backend::db::action_objects::MAX_CACHE_ENTRIES as i64
    {
        let entry: Option<(i64, i64)> = sqlx::query_as("SELECT id,size FROM actions_objects WHERE repo_id=$1 AND kind='cache' AND finalized ORDER BY accessed_at,id LIMIT 1")
            .bind(repo).fetch_optional(&mut *connection).await?;
        let Some((id, size)) = entry else {
            break;
        };
        sqlx::query("DELETE FROM actions_objects WHERE id=$1")
            .bind(id)
            .execute(&mut *connection)
            .await?;
        total -= size;
        count -= 1;
    }
    Ok(())
}

pub async fn find_cache(
    pool: &SqlitePool,
    run: &RuntimeRun,
    keys: &[String],
    version: &str,
) -> Result<Option<ActionObject>> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let sql = crate::backend::db::action_objects::cache_lookup_sql(false);
    let id: Option<i64> = sqlx::query_scalar(&sql)
        .bind(run.repo_id)
        .bind(version)
        .bind(chrono::Utc::now().timestamp())
        .bind(&run.branch)
        .bind(&run.default_branch)
        .bind(&run.base_branch)
        .bind(serde_json::to_string(keys)?)
        .fetch_optional(&mut *tx)
        .await?;
    let found = if let Some(id) = id {
        sqlx::query_as("UPDATE actions_objects SET expires_at=?2+(expires_at-accessed_at),accessed_at=?2 WHERE id=?1 RETURNING *")
            .bind(id).bind(chrono::Utc::now().timestamp()).fetch_optional(&mut *tx).await?
    } else {
        None
    };
    tx.commit().await?;
    Ok(found)
}

pub async fn list_artifacts(
    pool: &SqlitePool,
    run_id: i64,
    name: Option<&str>,
    id: Option<i64>,
) -> Result<Vec<crate::backend::db::action_objects::ArtifactSummary>> {
    Ok(sqlx::query_as("SELECT o.id,o.name,o.size,o.created_at FROM actions_objects o JOIN ci_runs a ON a.id=o.run_id JOIN ci_runs r ON r.id=$1 AND r.repo_id=o.repo_id AND COALESCE(a.workflow_run_id,a.id)=COALESCE(r.workflow_run_id,r.id) WHERE o.kind='artifact' AND o.finalized AND o.expires_at>$2 AND ($3 IS NULL OR o.name=$3) AND ($4 IS NULL OR o.id=$4) ORDER BY o.id")
        .bind(run_id).bind(chrono::Utc::now().timestamp()).bind(name).bind(id).fetch_all(pool).await?)
}

pub async fn artifact_for_run(
    pool: &SqlitePool,
    run_id: i64,
    name: &str,
) -> Result<Option<ActionObject>> {
    Ok(sqlx::query_as("SELECT o.* FROM actions_objects o JOIN ci_runs a ON a.id=o.run_id JOIN ci_runs r ON r.id=$1 AND r.repo_id=o.repo_id AND COALESCE(a.workflow_run_id,a.id)=COALESCE(r.workflow_run_id,r.id) WHERE o.kind='artifact' AND o.finalized AND o.expires_at>$2 AND o.name=$3 ORDER BY o.id DESC LIMIT 1")
        .bind(run_id).bind(chrono::Utc::now().timestamp()).bind(name).fetch_optional(pool).await?)
}

pub async fn artifact_for_reader(
    pool: &SqlitePool,
    actor: i64,
    session: &str,
    repo: i64,
    run: i64,
    id: i64,
) -> Result<Option<ActionObject>> {
    let access = crate::backend::db::sqlite::repositories::access::USER_REPO_ACCESS_SQL;
    Ok(sqlx::query_as(&format!("SELECT o.* FROM actions_objects o JOIN repositories r ON r.id=o.repo_id JOIN ci_runs c ON c.id=o.run_id AND c.repo_id=r.id JOIN users u ON u.id=?1 JOIN sessions s ON s.user_id=u.id AND s.id=?2 WHERE r.id=?3 AND c.id=?4 AND o.id=?5 AND o.kind='artifact' AND o.finalized AND o.expires_at>?6 AND s.expires_at>strftime('%Y-%m-%dT%H:%M:%f000Z','now') AND NOT u.is_banned AND NOT u.must_change_password AND ((r.owner_kind='user' AND u.is_admin) OR ({access})>0)"))
        .bind(actor).bind(session).bind(repo).bind(run).bind(id).bind(chrono::Utc::now().timestamp()).fetch_optional(pool).await?)
}

pub async fn artifact_by_id(pool: &SqlitePool, id: i64) -> Result<Option<ActionObject>> {
    Ok(sqlx::query_as("SELECT * FROM actions_objects WHERE id=$1 AND kind='artifact' AND finalized AND expires_at>$2")
        .bind(id).bind(chrono::Utc::now().timestamp()).fetch_optional(pool).await?)
}

pub async fn delete_artifact(
    pool: &SqlitePool,
    id: i64,
    run_id: i64,
    actor: Option<i64>,
    credential: Option<&crate::backend::db::ci::CiCredential>,
) -> Result<bool> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    if let Some(actor) = actor {
        let repo: Option<i64> = sqlx::query_scalar("SELECT repo_id FROM ci_runs WHERE id=$1")
            .bind(run_id)
            .fetch_optional(&mut *tx)
            .await?;
        let Some(repo) = repo else { return Ok(false) };
        if !crate::backend::db::sqlite::discussions::lock_write_authority(&mut tx, actor, repo)
            .await?
        {
            return Ok(false);
        }
        if let Some(proof) = credential {
            if !crate::backend::db::sqlite::credentials::ci(&mut tx, actor, repo, proof).await? {
                return Ok(false);
            }
        }
    }
    let changed =
        sqlx::query("DELETE FROM actions_objects WHERE id=$1 AND run_id=$2 AND kind='artifact'")
            .bind(id)
            .bind(run_id)
            .execute(&mut *tx)
            .await?
            .rows_affected()
            == 1;
    tx.commit().await?;
    Ok(changed)
}

fn object_query<'a>(
    filter: &'a crate::backend::db::action_objects::ObjectQuery<'_>,
    columns: &str,
) -> sqlx::QueryBuilder<'a, sqlx::Sqlite> {
    let mut query = sqlx::QueryBuilder::new(format!(
        "SELECT {columns} FROM actions_objects WHERE finalized AND repo_id="
    ));
    query
        .push_bind(filter.repo_id)
        .push(" AND kind=")
        .push_bind(filter.kind);
    if filter.kind == "cache" {
        query.push(" AND expires_at>CAST(strftime('%s','now') AS INTEGER)");
    }
    if let Some(run) = filter.run_id {
        query
            .push(" AND run_id IN (SELECT id FROM ci_runs WHERE COALESCE(workflow_run_id,id)=")
            .push_bind(run)
            .push(")");
    }
    if let Some(name) = filter.name {
        if filter.kind == "cache" {
            query
                .push(" AND substr(name,1,")
                .push_bind(name.chars().count() as i64)
                .push(")=")
                .push_bind(name);
        } else {
            query.push(" AND name=").push_bind(name);
        }
    }
    if let Some(scope) = filter.scope {
        query.push(" AND (scope=").push_bind(scope);
        if let Some(branch) = scope.strip_prefix("refs/heads/") {
            query.push(" OR scope=").push_bind(branch);
        }
        query.push(")");
    }
    query
}

pub async fn query_objects(
    pool: &SqlitePool,
    filter: &crate::backend::db::action_objects::ObjectQuery<'_>,
) -> Result<crate::backend::db::action_objects::ObjectPage> {
    use crate::backend::db::action_objects::ObjectOrder;
    use crate::backend::db::action_objects::ObjectPage;
    let mut tx = pool.begin().await?;
    let total = object_query(filter, "COUNT(*)")
        .build_query_scalar()
        .fetch_one(&mut *tx)
        .await?;
    let mut query = object_query(filter, "id,repo_id,run_id,kind,scope,name,version,size,finalized,NULL AS manifest,created_at,accessed_at,expires_at");
    let order = match filter.order {
        ObjectOrder::Created => "created_at",
        ObjectOrder::Accessed => "accessed_at",
        ObjectOrder::Size => "size",
    };
    query
        .push(" ORDER BY ")
        .push(order)
        .push(if filter.ascending {
            " ASC,id ASC"
        } else {
            " DESC,id DESC"
        })
        .push(" LIMIT ")
        .push_bind(filter.limit)
        .push(" OFFSET ")
        .push_bind(filter.offset);
    let objects = query.build_query_as().fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(ObjectPage { total, objects })
}

pub async fn object_by_id(
    pool: &SqlitePool,
    repo: i64,
    id: i64,
    kind: &str,
) -> Result<Option<ActionObject>> {
    Ok(sqlx::query_as(
        "SELECT * FROM actions_objects WHERE repo_id=$1 AND id=$2 AND kind=$3 AND finalized",
    )
    .bind(repo)
    .bind(id)
    .bind(kind)
    .fetch_optional(pool)
    .await?)
}

pub async fn delete_caches(
    pool: &SqlitePool,
    actor: i64,
    repo: i64,
    id: Option<i64>,
    key: Option<&str>,
    scope: Option<&str>,
) -> Result<Option<Vec<ActionObject>>> {
    anyhow::ensure!(
        id.is_some() || key.is_some(),
        "a cache id or key is required"
    );
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    if !crate::backend::db::sqlite::discussions::lock_write_authority(&mut tx, actor, repo).await? {
        return Ok(None);
    }
    let objects = sqlx::query_as("DELETE FROM actions_objects WHERE repo_id=$1 AND kind='cache' AND finalized AND ($2 IS NULL OR id=$2) AND ($3 IS NULL OR name=$3) AND ($4 IS NULL OR scope=$4 OR 'refs/heads/' || scope=$4) RETURNING *")
        .bind(repo).bind(id).bind(key).bind(scope).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(Some(objects))
}

pub async fn cache_usage(pool: &SqlitePool, repo: i64) -> Result<(i64, i64)> {
    Ok(sqlx::query_as("SELECT COUNT(*),COALESCE(SUM(size),0) FROM actions_objects WHERE repo_id=$1 AND kind='cache' AND finalized AND expires_at>CAST(strftime('%s','now') AS INTEGER)").bind(repo).fetch_one(pool).await?)
}

pub async fn grant_artifact_download(
    pool: &SqlitePool,
    actor: impl Into<crate::backend::db::credentials::Actor>,
    repo: i64,
    id: i64,
    hash: &str,
) -> Result<bool> {
    let actor = actor.into();
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let readable: bool = sqlx::query_scalar(&format!("SELECT EXISTS(SELECT 1 FROM repositories r JOIN users u ON u.id=$1 WHERE r.id=$2 AND NOT u.is_banned AND NOT u.must_change_password AND {})", crate::backend::db::sqlite::repositories::access::readable_condition()))
        .bind(actor.user_id).bind(repo).fetch_one(&mut *tx).await?;
    if !readable {
        return Ok(false);
    }
    if !crate::backend::db::sqlite::credentials::repository(&mut tx, &actor, repo, "actions", false)
        .await?
    {
        return Ok(false);
    }
    let changed = sqlx::query("INSERT INTO actions_blob_grants(token_hash,object_id,run_id,write,expires_at) SELECT $1,id,run_id,FALSE,CAST(strftime('%s','now') AS INTEGER)+60 FROM actions_objects WHERE id=$2 AND repo_id=$3 AND kind='artifact' AND finalized AND expires_at>CAST(strftime('%s','now') AS INTEGER)")
        .bind(hash).bind(id).bind(repo).execute(&mut *tx).await?.rows_affected()==1;
    tx.commit().await?;
    Ok(changed)
}

pub async fn expire_objects(pool: &SqlitePool) -> Result<()> {
    let now = chrono::Utc::now().timestamp();
    sqlx::query("DELETE FROM actions_objects WHERE id IN (SELECT id FROM actions_objects WHERE expires_at<=$1 OR (NOT finalized AND created_at<$2) ORDER BY id LIMIT 100)")
        .bind(now).bind(now-86400).execute(pool).await?;
    sqlx::query("DELETE FROM actions_blob_grants WHERE token_hash IN (SELECT token_hash FROM actions_blob_grants WHERE expires_at<=?1 ORDER BY expires_at,token_hash LIMIT 1000)")
        .bind(now).execute(pool).await?;
    crate::backend::db::sqlite::action_logs::expire(pool).await?;
    Ok(())
}
