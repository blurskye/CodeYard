use crate::backend::db::action_objects::{ActionObject, BlobPart, NewObject, RuntimeRun};
use anyhow::Result;
use sqlx::PgPool;

pub async fn issue_token(pool: &PgPool, run_id: i64, hash: &str, backend_id: &str) -> Result<()> {
    let changed = sqlx::query("INSERT INTO actions_runtime_tokens(run_id,token_hash,backend_id) SELECT id,$2,$3 FROM ci_runs WHERE id=$1 AND status='running' AND NOT cancel_requested ON CONFLICT(run_id) DO UPDATE SET token_hash=excluded.token_hash,backend_id=excluded.backend_id")
        .bind(run_id).bind(hash).bind(backend_id).execute(pool).await?;
    anyhow::ensure!(changed.rows_affected() == 1, "run is not active");
    Ok(())
}

pub async fn authenticate(pool: &PgPool, hash: &str) -> Result<Option<RuntimeRun>> {
    Ok(sqlx::query_as("SELECT r.id,r.repo_id,t.backend_id,CASE WHEN r.event='pull_request' THEN 'refs/pull/' || COALESCE(CAST(pr.number AS TEXT),CAST(r.id AS TEXT)) || '/merge' ELSE r.branch END AS branch,CASE WHEN r.event='pull_request' THEN COALESCE(r.pull_request_event->>'base_ref',pr.base_branch) END AS base_branch,p.default_branch,r.is_fork FROM actions_runtime_tokens t JOIN ci_runs r ON r.id=t.run_id JOIN repositories p ON p.id=r.repo_id LEFT JOIN pull_requests pr ON pr.id=r.pull_request_id WHERE t.token_hash=$1 AND (r.status='running' OR r.completed_at > NOW() - INTERVAL '5 minutes')")
        .bind(hash).fetch_optional(pool).await?)
}

pub async fn create_object(pool: &PgPool, new: NewObject<'_>) -> Result<Option<ActionObject>> {
    // Fork writes belong only to their derived pull-request merge-ref scope.
    if new.kind == "cache" && new.run.is_fork && !(new.run.branch.starts_with("refs/pull/") && new.run.branch.ends_with("/merge")) {
        return Ok(None);
    }
    let now = chrono::Utc::now().timestamp();
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT id FROM repositories WHERE id=$1 FOR UPDATE")
        .bind(new.run.repo_id)
        .execute(&mut *tx)
        .await?;
    let expires_at = if new.kind == "cache" {
        new.expires_at
            .min(now + i64::from(cache_policy(&mut tx, new.run.repo_id).await?.1) * 86400)
    } else {
        new.expires_at
    };
    // An expired key can be reserved again without waiting for the background sweep.
    sqlx::query("DELETE FROM actions_objects WHERE repo_id=$1 AND kind=$2 AND name=$3 AND version=$4 AND (run_id=$5 OR (kind='cache' AND scope=$6)) AND (expires_at<=$7 OR (NOT finalized AND EXISTS(SELECT 1 FROM ci_runs c WHERE c.id=actions_objects.run_id AND c.status NOT IN ('queued','running') AND c.completed_at<clock_timestamp()-INTERVAL '5 minutes')))")
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
    pool: &PgPool,
    run_id: i64,
    kind: &str,
    name: &str,
    version: &str,
) -> Result<Option<ActionObject>> {
    Ok(sqlx::query_as("SELECT * FROM actions_objects WHERE run_id=$1 AND kind=$2 AND name=$3 AND version=$4 AND expires_at>$5")
        .bind(run_id).bind(kind).bind(name).bind(version).bind(chrono::Utc::now().timestamp()).fetch_optional(pool).await?)
}

pub async fn grant_blob(
    pool: &PgPool,
    object_id: i64,
    run_id: i64,
    hash: &str,
    write: bool,
) -> Result<bool> {
    // Only while the run keeps its runtime token: losing access deletes the token and the
    // grants together. The share lock waits for such a deletion, so a request that
    // authenticated before it cannot grant afterwards.
    let mut tx = pool.begin().await?;
    let live: Option<i32> =
        sqlx::query_scalar("SELECT 1 FROM actions_runtime_tokens WHERE run_id=$1 FOR SHARE")
            .bind(run_id)
            .fetch_optional(&mut *tx)
            .await?;
    if live.is_none() {
        return Ok(false);
    }
    sqlx::query("INSERT INTO actions_blob_grants(token_hash,object_id,run_id,write,expires_at) VALUES($1,$2,$3,$4,$5)")
        .bind(hash).bind(object_id).bind(run_id).bind(write).bind(chrono::Utc::now().timestamp()+3600).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(true)
}

pub async fn blob_grant(
    pool: &PgPool,
    object_id: i64,
    hash: &str,
    write: bool,
) -> Result<Option<ActionObject>> {
    Ok(sqlx::query_as("SELECT o.* FROM actions_blob_grants g JOIN actions_objects o ON o.id=g.object_id JOIN ci_runs r ON r.id=g.run_id WHERE o.id=$1 AND g.token_hash=$2 AND g.write=$3 AND g.expires_at>$4 AND o.expires_at>$4 AND (NOT $3 OR (r.status='running' OR r.completed_at > NOW() - INTERVAL '5 minutes'))")
        .bind(object_id).bind(hash).bind(write).bind(chrono::Utc::now().timestamp()).fetch_optional(pool).await?)
}

pub async fn stage_block(
    pool: &PgPool,
    object_id: i64,
    part: &BlobPart,
    limit: i64,
) -> Result<bool> {
    let mut tx = pool.begin().await?;
    let repo: Option<i64> = sqlx::query_scalar("SELECT repo_id FROM actions_objects WHERE id=$1")
        .bind(object_id)
        .fetch_optional(&mut *tx)
        .await?;
    let Some(repo) = repo else { return Ok(false) };
    // Lock the repository before the object, as finalize does, so concurrent uploads see each other's bytes.
    sqlx::query("SELECT id FROM repositories WHERE id=$1 FOR UPDATE")
        .bind(repo)
        .execute(&mut *tx)
        .await?;
    let object: Option<ActionObject> =
        sqlx::query_as("SELECT * FROM actions_objects WHERE id=$1 FOR UPDATE")
            .bind(object_id)
            .fetch_optional(&mut *tx)
            .await?;
    let Some(object) = object.filter(|o| o.manifest.is_none() && !o.finalized) else {
        return Ok(false);
    };
    let total: i64 = sqlx::query_scalar(
        "SELECT CAST(COALESCE(SUM(size),0) AS BIGINT) FROM actions_blob_parts WHERE object_id=$1 AND block_id<>$2",
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
            "SELECT CAST(COALESCE(SUM(p.size),0) AS BIGINT) FROM actions_blob_parts p JOIN actions_objects o ON o.id=p.object_id WHERE o.repo_id=$1 AND o.kind='cache' AND NOT o.finalized AND NOT (p.object_id=$2 AND p.block_id=$3)",
        )
        .bind(repo)
        .bind(object.id)
        .bind(&part.block_id)
        .fetch_one(&mut *tx)
        .await?;
        let room = limit - pending - part.size;
        if room < 0 {
            return Ok(false);
        }
        enforce_cache_limit(&mut tx, repo, room).await?;
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

pub async fn ready_block(pool: &PgPool, object_id: i64, key: &str) -> Result<bool> {
    Ok(
        sqlx::query(
            "UPDATE actions_blob_parts SET ready=TRUE WHERE object_id=$1 AND object_key=$2",
        )
        .bind(object_id)
        .bind(key)
        .execute(pool)
        .await?
        .rows_affected()
            == 1,
    )
}

/// Clear the block list of an upload that is not finalized, so a whole-blob PUT can stage it again.
pub async fn reopen_blob(pool: &PgPool, object_id: i64) -> Result<()> {
    sqlx::query("UPDATE actions_objects SET manifest=NULL WHERE id=$1 AND NOT finalized")
        .bind(object_id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn seal_blob(pool: &PgPool, object_id: i64, blocks: &[String]) -> Result<bool> {
    let mut tx = pool.begin().await?;
    let object: Option<ActionObject> =
        sqlx::query_as("SELECT * FROM actions_objects WHERE id=$1 FOR UPDATE")
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
    sqlx::query("UPDATE actions_objects SET manifest=$2,size=$3 WHERE id=$1")
        .bind(object_id)
        .bind(serde_json::to_string(&parts)?)
        .bind(size)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(true)
}

pub async fn finalize_object(
    pool: &PgPool,
    run_id: i64,
    hash: &str,
    object_id: i64,
    size: i64,
    cache_limit: i64,
) -> Result<bool> {
    let mut tx = pool.begin().await?;
    // Serialize the repository's finalizations so concurrent uploads cannot exceed its cache quota.
    let repo: Option<i64> =
        sqlx::query_scalar("SELECT repo_id FROM actions_objects WHERE id=$1 AND run_id=$2")
            .bind(object_id)
            .bind(run_id)
            .fetch_optional(&mut *tx)
            .await?;
    let Some(repo) = repo else { return Ok(false) };
    sqlx::query("SELECT id FROM repositories WHERE id=$1 FOR UPDATE")
        .bind(repo)
        .execute(&mut *tx)
        .await?;
    let live: Option<i64> = sqlx::query_scalar("SELECT t.run_id FROM actions_runtime_tokens t JOIN ci_runs r ON r.id=t.run_id WHERE t.run_id=$1 AND t.token_hash=$2 AND NOT r.cancel_requested AND (r.status='running' OR r.completed_at>clock_timestamp()-INTERVAL '5 minutes') FOR SHARE OF t")
        .bind(run_id).bind(hash).fetch_optional(&mut *tx).await?;
    if live.is_none() {
        return Ok(false);
    }
    let object: ActionObject =
        sqlx::query_as("SELECT * FROM actions_objects WHERE id=$1 FOR UPDATE")
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
    connection: &mut sqlx::PgConnection,
    repo: i64,
) -> Result<(i64, i32)> {
    let (gib,days):(i32,i32)=sqlx::query_as("SELECT LEAST(COALESCE((SELECT cache_size_gib FROM actions_settings WHERE id=1),10),COALESCE((SELECT cache_size_gib FROM repo_actions_settings WHERE repo_id=$1),100000),COALESCE((SELECT s.cache_size_gib FROM org_actions_settings s JOIN repositories r ON r.owner_org_id=s.org_id AND r.owner_kind='org' WHERE r.id=$1),100000)),LEAST(COALESCE((SELECT cache_retention_days FROM actions_settings WHERE id=1),7),COALESCE((SELECT cache_retention_days FROM repo_actions_settings WHERE repo_id=$1),365),COALESCE((SELECT s.cache_retention_days FROM org_actions_settings s JOIN repositories r ON r.owner_org_id=s.org_id AND r.owner_kind='org' WHERE r.id=$1),365))").bind(repo).fetch_one(connection).await?;
    Ok((i64::from(gib) * 1024 * 1024 * 1024, days))
}

/// Apply the repository's current cache size and retention limits to its existing caches.
pub(crate) async fn apply_cache_policy(
    connection: &mut sqlx::PgConnection,
    repo: i64,
) -> Result<()> {
    let (bytes, days) = cache_policy(&mut *connection, repo).await?;
    enforce_cache_limit(&mut *connection, repo, bytes).await?;
    sqlx::query("UPDATE actions_objects SET expires_at=LEAST(expires_at,accessed_at+$2) WHERE repo_id=$1 AND kind='cache'")
        .bind(repo)
        .bind(i64::from(days) * 86400)
        .execute(&mut *connection)
        .await?;
    Ok(())
}

pub(crate) async fn enforce_cache_limit(
    connection: &mut sqlx::PgConnection,
    repo: i64,
    cache_limit: i64,
) -> Result<()> {
    let mut total: i64 = sqlx::query_scalar("SELECT CAST(COALESCE(SUM(size),0) AS BIGINT) FROM actions_objects WHERE repo_id=$1 AND kind='cache' AND finalized")
        .bind(repo).fetch_one(&mut *connection).await?;
    while total > cache_limit {
        let entry: Option<(i64, i64)> = sqlx::query_as("SELECT id,size FROM actions_objects WHERE repo_id=$1 AND kind='cache' AND finalized ORDER BY accessed_at,id LIMIT 1")
            .bind(repo).fetch_optional(&mut *connection).await?;
        let Some((id, size)) = entry else { break; };
        sqlx::query("DELETE FROM actions_objects WHERE id=$1")
            .bind(id)
            .execute(&mut *connection)
            .await?;
        total -= size;
    }
    Ok(())
}

pub async fn find_cache(
    pool: &PgPool,
    run: &RuntimeRun,
    keys: &[String],
    version: &str,
) -> Result<Option<ActionObject>> {
    let mut tx = pool.begin().await?;
    let entries: Vec<ActionObject> = sqlx::query_as("SELECT * FROM actions_objects WHERE repo_id=$1 AND kind='cache' AND version=$2 AND finalized AND expires_at>$3 AND (scope=$4 OR scope=$5 OR scope=$6) ORDER BY created_at DESC,id DESC")
        .bind(run.repo_id).bind(version).bind(chrono::Utc::now().timestamp()).bind(&run.branch).bind(&run.default_branch).bind(&run.base_branch).fetch_all(&mut *tx).await?;
    let found = crate::backend::db::action_objects::matching_cache(&entries, run, keys);
    if let Some(entry) = found {
        sqlx::query("UPDATE actions_objects SET expires_at=$2+(expires_at-accessed_at),accessed_at=$2 WHERE id=$1")
            .bind(entry.id)
            .bind(chrono::Utc::now().timestamp())
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(found.cloned())
}

pub async fn list_artifacts(pool: &PgPool, run_id: i64) -> Result<Vec<ActionObject>> {
    Ok(sqlx::query_as("SELECT * FROM actions_objects WHERE run_id=$1 AND kind='artifact' AND finalized AND expires_at>$2 ORDER BY id")
        .bind(run_id).bind(chrono::Utc::now().timestamp()).fetch_all(pool).await?)
}

pub async fn artifact_by_id(pool: &PgPool, id: i64) -> Result<Option<ActionObject>> {
    Ok(sqlx::query_as("SELECT * FROM actions_objects WHERE id=$1 AND kind='artifact' AND finalized AND expires_at>$2")
        .bind(id).bind(chrono::Utc::now().timestamp()).fetch_optional(pool).await?)
}

pub async fn delete_artifact(
    pool: &PgPool,
    id: i64,
    run_id: i64,
    actor: Option<i64>,
) -> Result<bool> {
    let mut tx = pool.begin().await?;
    if let Some(actor) = actor {
        let repo: Option<i64> = sqlx::query_scalar("SELECT repo_id FROM ci_runs WHERE id=$1")
            .bind(run_id)
            .fetch_optional(&mut *tx)
            .await?;
        let Some(repo) = repo else { return Ok(false) };
        if !super::discussions::lock_write_authority(&mut tx, actor, repo).await? {
            return Ok(false);
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
) -> sqlx::QueryBuilder<'a, sqlx::Postgres> {
    let mut query = sqlx::QueryBuilder::new(format!(
        "SELECT {columns} FROM actions_objects WHERE finalized AND repo_id="
    ));
    query
        .push_bind(filter.repo_id)
        .push(" AND kind=")
        .push_bind(filter.kind);
    if filter.kind == "cache" {
        query.push(" AND expires_at>FLOOR(EXTRACT(EPOCH FROM transaction_timestamp()))::bigint");
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
                .push_bind(name.chars().count() as i32)
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
    pool: &PgPool,
    filter: &crate::backend::db::action_objects::ObjectQuery<'_>,
) -> Result<crate::backend::db::action_objects::ObjectPage> {
    use crate::backend::db::action_objects::{ObjectOrder, ObjectPage};
    let mut tx = pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
        .execute(&mut *tx)
        .await?;
    let total = object_query(filter, "COUNT(*)")
        .build_query_scalar()
        .fetch_one(&mut *tx)
        .await?;
    let mut query = object_query(filter, "*");
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
    pool: &PgPool,
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
    pool: &PgPool,
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
    let mut tx = pool.begin().await?;
    if !super::discussions::lock_write_authority(&mut tx, actor, repo).await? {
        return Ok(None);
    }
    let objects = sqlx::query_as("DELETE FROM actions_objects WHERE repo_id=$1 AND kind='cache' AND finalized AND ($2::bigint IS NULL OR id=$2) AND ($3::text IS NULL OR name=$3) AND ($4::text IS NULL OR scope=$4 OR 'refs/heads/' || scope=$4) RETURNING *")
        .bind(repo).bind(id).bind(key).bind(scope).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(Some(objects))
}

pub async fn cache_usage(pool: &PgPool, repo: i64) -> Result<(i64, i64)> {
    Ok(sqlx::query_as("SELECT COUNT(*),COALESCE(SUM(size),0)::bigint FROM actions_objects WHERE repo_id=$1 AND kind='cache' AND finalized AND expires_at>FLOOR(EXTRACT(EPOCH FROM clock_timestamp()))::bigint").bind(repo).fetch_one(pool).await?)
}

pub async fn grant_artifact_download(
    pool: &PgPool,
    actor: i64,
    repo: i64,
    id: i64,
    hash: &str,
) -> Result<bool> {
    use super::repositories::access::{lock_user_repo_access_in_transaction, RepositoryRowLock};
    let mut tx = pool.begin().await?;
    let authority =
        lock_user_repo_access_in_transaction(&mut tx, actor, repo, RepositoryRowLock::Shared, true)
            .await?;
    if !authority.is_some_and(|a| a.access.can_read() || (!a.organization && a.is_admin)) {
        return Ok(false);
    }
    let changed = sqlx::query("INSERT INTO actions_blob_grants(token_hash,object_id,run_id,write,expires_at) SELECT $1,id,run_id,FALSE,FLOOR(EXTRACT(EPOCH FROM clock_timestamp()))::bigint+60 FROM actions_objects WHERE id=$2 AND repo_id=$3 AND kind='artifact' AND finalized AND expires_at>FLOOR(EXTRACT(EPOCH FROM clock_timestamp()))::bigint")
        .bind(hash).bind(id).bind(repo).execute(&mut *tx).await?.rows_affected()==1;
    tx.commit().await?;
    Ok(changed)
}

pub async fn expire_objects(pool: &PgPool) -> Result<()> {
    let now = chrono::Utc::now().timestamp();
    sqlx::query("DELETE FROM actions_objects WHERE id IN (SELECT id FROM actions_objects WHERE expires_at<=$1 OR (NOT finalized AND created_at<$2) ORDER BY id LIMIT 100)")
        .bind(now).bind(now-86400).execute(pool).await?;
    sqlx::query("DELETE FROM actions_blob_grants WHERE expires_at<=$1")
        .bind(now)
        .execute(pool)
        .await?;
    Ok(())
}
