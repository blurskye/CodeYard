use crate::models::AccessLevel;
use anyhow::Result;
use sqlx::SqliteConnection;
/// Effectful callers use BEGIN IMMEDIATE and recheck before durable admission.
/// A pool projection alone is never a write permit.
pub async fn user_repo_access_in_transaction(
    connection: &mut SqliteConnection,
    user_id: i64,
    repo_id: i64,
) -> Result<AccessLevel> {
    // Projection only, not a write permit. All contributions and governance
    // share one statement snapshot; suspended orgs cap even nonmember grants.
    // Collaborator levels are 1..=5; Owner comes only from scoped ownership.
    let level: Option<i16> = sqlx::query_scalar(&format!(
        "SELECT ({USER_REPO_ACCESS_SQL}) FROM repositories r JOIN users u ON u.id = ?1 WHERE r.id = ?2"
    ))
    .bind(user_id)
    .bind(repo_id)
    .fetch_optional(connection)
    .await?;
    Ok(AccessLevel::from(level.unwrap_or(0)))
}

// The actor's access level on repository `r`, 0 when none. A plain scalar over the
// outer `r` and `u` rows, so the run list can evaluate it once per repository.
pub const USER_REPO_ACCESS_SQL: &str = r#"
    (CASE WHEN NOT r.is_gist AND NOT u.is_banned AND NOT u.must_change_password
        AND ((r.owner_kind = 'user' AND r.owner_user_id IS NOT NULL AND r.owner_org_id IS NULL)
            OR (r.owner_kind = 'org' AND r.owner_user_id IS NULL AND r.owner_org_id IS NOT NULL
                AND EXISTS (SELECT 1 FROM organizations o WHERE o.id = r.owner_org_id)))
        AND NOT EXISTS (SELECT 1 FROM organization_blocks b WHERE b.org_id=r.owner_org_id AND b.user_id=u.id)
        THEN MIN(MAX(
            CASE WHEN r.owner_kind = 'user' AND r.owner_user_id = u.id
                AND r.owner_org_id IS NULL THEN 6 ELSE 0 END,
            CASE WHEN NOT r.is_private THEN 1 ELSE 0 END,
            COALESCE((SELECT MAX(a.access_level) FROM github_apps a
                JOIN github_app_installations i ON i.app_id=a.id
                JOIN users owner ON owner.id=a.owner_id
                WHERE a.bot_id=u.id AND i.suspended_at IS NULL
                    AND NOT owner.is_banned AND NOT owner.must_change_password
                    AND (r.owner_user_id=i.user_id OR r.owner_org_id=i.org_id)
                    AND (i.all_repos OR EXISTS(SELECT 1 FROM github_app_repositories ar
                        WHERE ar.installation_id=i.id AND ar.repo_id=r.id))),0),
            COALESCE((SELECT d.access_level FROM repo_access d
                WHERE d.repo_id = r.id AND d.user_id = u.id
                    AND d.access_level BETWEEN 1 AND 5), 0),
            COALESCE((SELECT s.default_repository_permission FROM organization_settings s
                JOIN org_memberships member ON member.org_id=s.org_id AND member.user_id=u.id
                    AND member.role IN ('owner','admin','member')
                WHERE s.org_id=r.owner_org_id AND s.default_repository_permission IN (1,3,5)),0),
            COALESCE((SELECT MAX(role.base_level) FROM organization_role_assignments a
                JOIN organization_roles role ON role.id=a.role_id AND role.base_level BETWEEN 1 AND 5
                WHERE a.org_id=r.owner_org_id
                    AND EXISTS(SELECT 1 FROM org_memberships member WHERE member.org_id=a.org_id
                        AND member.user_id=u.id AND member.role IN ('owner','admin','member'))
                    AND ((a.user_id=u.id AND a.team_id IS NULL) OR (a.user_id IS NULL AND EXISTS(
                        SELECT 1 FROM teams t JOIN team_ancestors lineage ON lineage.ancestor_id=t.id
                        JOIN team_memberships tm ON tm.team_id=lineage.team_id
                        WHERE t.id=a.team_id AND t.org_id=a.org_id AND t.privacy IN ('visible','secret')
                            AND tm.user_id=u.id AND tm.role IN ('member','maintainer'))))),0),
            COALESCE((
                SELECT CASE m.role WHEN 'owner' THEN 6 WHEN 'admin' THEN 5
                    WHEN 'member' THEN COALESCE((
                        SELECT MAX(a.permission) FROM team_repo_access a
                        JOIN teams t ON t.id = a.team_id AND t.org_id = m.org_id
                        JOIN team_ancestors lineage ON lineage.ancestor_id=t.id
                        JOIN team_memberships tm ON tm.team_id=lineage.team_id
                            AND tm.user_id = u.id AND tm.role IN ('member','maintainer')
                        WHERE a.repo_id = r.id AND a.permission BETWEEN 1 AND 5
                            AND t.privacy IN ('visible','secret')
                    ), 0) ELSE 0 END
                FROM organizations o JOIN org_memberships m ON m.org_id = o.id
                WHERE r.owner_kind = 'org' AND r.owner_user_id IS NULL
                    AND o.id = r.owner_org_id AND m.user_id = u.id
            ), 0)),
            CASE WHEN r.owner_kind = 'org' AND NOT EXISTS (
                    SELECT 1 FROM organizations o
                    JOIN org_memberships m ON m.org_id = o.id AND m.role = 'owner'
                    JOIN users own ON own.id = m.user_id
                    WHERE o.id = r.owner_org_id
                        AND NOT own.is_banned AND NOT own.must_change_password)
                THEN 1 ELSE 6 END)
        ELSE 0 END)
    "#;

/// Whether repository `r` is readable by the viewer, whom the query joins as `u` (NULL for an
/// anonymous visitor). It follows `repositories::access::can_read`: public repositories, the
/// personal repositories of a site admin, and the access projection for everything else.
pub fn readable_condition() -> String {
    format!(
        "(NOT r.is_gist AND (NOT r.is_private OR (r.owner_kind = 'user' AND u.is_admin AND NOT u.is_banned AND NOT u.must_change_password) OR ({USER_REPO_ACCESS_SQL}) >= 1))"
    )
}

/// Read-only callers retain one snapshot; effectful callers reserve BEGIN IMMEDIATE first.
/// Archival and interaction restrictions do not remove repository read access.
pub async fn lock_readable_repository_in_transaction(
    connection: &mut sqlx::SqliteConnection,
    user_id: i64,
    repo_id: i64,
) -> Result<bool> {
    Ok(sqlx::query_scalar(&format!("SELECT EXISTS(SELECT 1 FROM repositories r JOIN users u ON u.id=?1 WHERE r.id=?2 AND NOT u.is_banned AND NOT u.must_change_password AND {})", readable_condition()))
        .bind(user_id).bind(repo_id).fetch_one(connection).await?)
}
