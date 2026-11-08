use anyhow::Result;
pub async fn cache_policy(
    connection: &mut sqlx::SqliteConnection,
    repo: i64,
) -> Result<(i64, i32)> {
    let (site_size,site_days,repo_size,repo_days,org_size,org_days): crate::queries::action_objects::CacheSettings = sqlx::query_as("SELECT (SELECT cache_size_gib FROM actions_settings WHERE id=1),(SELECT cache_retention_days FROM actions_settings WHERE id=1),(SELECT cache_size_gib FROM repo_actions_settings WHERE repo_id=$1),(SELECT cache_retention_days FROM repo_actions_settings WHERE repo_id=$1),(SELECT s.cache_size_gib FROM org_actions_settings s JOIN repositories r ON r.owner_org_id=s.org_id AND r.owner_kind='org' WHERE r.id=$1),(SELECT s.cache_retention_days FROM org_actions_settings s JOIN repositories r ON r.owner_org_id=s.org_id AND r.owner_kind='org' WHERE r.id=$1)").bind(repo).fetch_one(connection).await?;
    use crate::queries::actions::CacheLimit;
    Ok((
        i64::from(CacheLimit::Storage.effective(site_size, org_size, repo_size))
            * 1024
            * 1024
            * 1024,
        CacheLimit::Retention.effective(site_days, org_days, repo_days),
    ))
}

pub async fn enforce_cache_limit(
    connection: &mut sqlx::SqliteConnection,
    repo: i64,
    cache_limit: i64,
) -> Result<()> {
    let (mut total, mut count): (i64, i64) = sqlx::query_as("SELECT CAST(COALESCE(SUM(size),0) AS BIGINT),COUNT(*) FROM actions_objects WHERE repo_id=$1 AND kind='cache' AND finalized")
        .bind(repo).fetch_one(&mut *connection).await?;
    while total > cache_limit || count > crate::queries::action_objects::MAX_CACHE_ENTRIES as i64 {
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
