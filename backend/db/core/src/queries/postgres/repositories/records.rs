use anyhow::Result;
/// The ancestors of the fork bound to $1, found by walking forked_from_id upward.
pub const ANCESTORS: &str = "WITH RECURSIVE ancestors(id) AS (
    SELECT forked_from_id FROM repositories WHERE id = $1 AND forked_from_id IS NOT NULL
    UNION
    SELECT r.forked_from_id FROM repositories r JOIN ancestors a ON r.id = a.id
    WHERE r.forked_from_id IS NOT NULL
) SELECT id FROM ancestors";

/// Locks the ancestors of the fork bound to `repo_id`, before the fork's own row. This is
/// the one lock order for a fork and its sources: the organizations that own the sources
/// FOR SHARE, then the sources by id (a source always has a lower id than its fork), each
/// with `lock`. Reads take FOR SHARE. A deletion or privacy change takes FOR NO KEY UPDATE,
/// the lock its forks_count trigger needs on the source, so it never upgrades a shared
/// source lock that another transaction holds. Authority checks call this through
/// `lock_user_repo_access_in_transaction`, before they lock the fork.
pub async fn lock_fork_ancestors(
    connection: &mut sqlx::PgConnection,
    repo_id: i64,
    lock: &str,
) -> Result<()> {
    let sql = format!(
        "SELECT id FROM organizations WHERE id IN (
            SELECT owner_org_id FROM repositories
            WHERE owner_kind = 'org' AND id IN ({ANCESTORS})
        ) ORDER BY id FOR SHARE"
    );
    let _: Vec<i64> = sqlx::query_scalar(&sql)
        .bind(repo_id)
        .fetch_all(&mut *connection)
        .await?;
    let sql = format!("SELECT id FROM repositories WHERE id IN ({ANCESTORS}) ORDER BY id {lock}");
    let _: Vec<i64> = sqlx::query_scalar(&sql)
        .bind(repo_id)
        .fetch_all(connection)
        .await?;
    Ok(())
}
