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

/// Drops what the former owner granted or attached to a repository whose owner changed:
/// direct grants an organization cannot manage, collaborators the new personal owner
/// blocks, team, runner group and organization value grants, pending field suggestions,
/// maintainer edits of its pull requests, the security configuration link, remembered
/// secret scans and artifact inventory. A transfer within the cell and a move to an owner
/// in another cell both run it after changing the owner.
pub async fn leave_former_owner(
    connection: &mut sqlx::PgConnection,
    repo_id: i64,
    personal_owner: Option<i64>,
) -> Result<()> {
    match personal_owner {
        None => {
            sqlx::query("DELETE FROM repo_access WHERE repo_id=$1")
                .bind(repo_id)
                .execute(&mut *connection)
                .await?;
        }
        Some(owner) => {
            sqlx::query("DELETE FROM repo_access a WHERE a.repo_id=$1 AND EXISTS(SELECT 1 FROM user_blocks b WHERE (b.user_id=$2 AND b.blocked_id=a.user_id) OR (b.blocked_id=$2 AND b.user_id=a.user_id))")
                .bind(repo_id)
                .bind(owner)
                .execute(&mut *connection)
                .await?;
        }
    }
    for statement in [
        "DELETE FROM action_runner_group_repositories WHERE repo_id=$1",
        "UPDATE issue_suggestions SET state='invalidated',updated_at=clock_timestamp() WHERE state='pending' AND action='add_field' AND issue_id IN (SELECT id FROM issues WHERE repo_id=$1)",
        "UPDATE pull_requests SET maintainer_can_modify=FALSE,maintainer_edit_granted_by_id=NULL,updated_at=GREATEST(clock_timestamp(),updated_at+INTERVAL '1 microsecond') WHERE head_repo_id=$1 AND maintainer_can_modify",
        // Selected organization values must not silently regain access on a transfer back.
        "DELETE FROM org_actions_value_repositories WHERE repo_id=$1",
        // Detach the former owner's policy without discarding already applied settings.
        "UPDATE repository_security_settings SET configuration_id=NULL WHERE repo_id=$1",
        "DELETE FROM secret_scanning_commits WHERE repo_id=$1",
        // Inventory describes the former namespace. Do not transfer it or reserve its deployment names.
        "DELETE FROM artifact_metadata_records WHERE artifact_id IN (SELECT id FROM artifact_metadata WHERE repo_id=$1)",
        // Team grants belong to the former owner's teams and must not come back later.
        "DELETE FROM pr_team_reviewers WHERE pull_request_id IN (SELECT id FROM pull_requests WHERE repo_id=$1)",
        "DELETE FROM team_repo_access WHERE repo_id=$1",
    ] {
        sqlx::query(statement)
            .bind(repo_id)
            .execute(&mut *connection)
            .await?;
    }
    Ok(())
}
