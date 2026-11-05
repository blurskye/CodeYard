use crate::repositories::push_teams::validate_count;
use crate::repositories::push_teams::TeamAccess;
use anyhow::Result;
use codeyard_db_core::models::Team;
use codeyard_db_core::pool::PgPool;
use sqlx::PgConnection;

const PERMISSION:&str="GREATEST(CASE WHEN a.permission BETWEEN 1 AND 5 THEN a.permission ELSE 0 END,COALESCE((SELECT s.default_repository_permission FROM organization_settings s WHERE s.org_id=t.org_id AND s.default_repository_permission IN (1,3,5)),0),COALESCE((SELECT MAX(role.base_level) FROM organization_role_assignments assigned JOIN organization_roles role ON role.id=assigned.role_id AND role.base_level BETWEEN 1 AND 5 JOIN team_ancestors ancestor ON ancestor.ancestor_id=assigned.team_id AND ancestor.team_id=t.id WHERE assigned.org_id=t.org_id AND assigned.user_id IS NULL),0))";

pub async fn by_names(pool: &PgPool, repo: i64, names: &[String]) -> Result<Vec<Team>> {
    validate_count(names.len())?;
    Ok(sqlx::query_as("SELECT t.* FROM teams t JOIN repositories r ON r.owner_org_id=t.org_id AND r.owner_kind='org' AND r.owner_user_id IS NULL WHERE r.id=$1 AND t.slug=ANY($2) AND t.privacy IN ('visible','secret') ORDER BY t.id").bind(repo).bind(names).fetch_all(pool).await?)
}
pub async fn by_ids(pool: &PgPool, repo: i64, ids: &[i64]) -> Result<Vec<TeamAccess>> {
    validate_count(ids.len())?;
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    Ok(sqlx::query_as(&format!("SELECT t.*,o.name AS org_slug,({PERMISSION})::SMALLINT AS permission FROM teams t JOIN repositories r ON r.owner_org_id=t.org_id AND r.owner_kind='org' AND r.owner_user_id IS NULL JOIN organizations o ON o.id=t.org_id LEFT JOIN effective_team_repo_access a ON a.team_id=t.id AND a.repo_id=r.id WHERE r.id=$1 AND t.id=ANY($2) AND t.privacy IN ('visible','secret') ORDER BY t.id")).bind(repo).bind(ids).fetch_all(pool).await?)
}
pub(super) async fn eligible_ids_on(
    connection: &mut PgConnection,
    repo: i64,
    ids: &[i64],
) -> Result<Vec<i64>> {
    validate_count(ids.len())?;
    Ok(sqlx::query_scalar(&format!("SELECT t.id FROM teams t JOIN repositories r ON r.owner_org_id=t.org_id AND r.owner_kind='org' AND r.owner_user_id IS NULL LEFT JOIN effective_team_repo_access a ON a.team_id=t.id AND a.repo_id=r.id WHERE r.id=$1 AND t.id=ANY($2) AND t.privacy IN ('visible','secret') AND ({PERMISSION})>=3 ORDER BY t.id")).bind(repo).bind(ids).fetch_all(connection).await?)
}
pub async fn contains_actor(pool: &PgPool, repo: i64, actor: i64, ids: &[i64]) -> Result<bool> {
    validate_count(ids.len())?;
    Ok(sqlx::query_scalar(&format!("SELECT EXISTS(SELECT 1 FROM teams t JOIN repositories r ON r.owner_org_id=t.org_id AND r.owner_kind='org' AND r.owner_user_id IS NULL LEFT JOIN effective_team_repo_access a ON a.team_id=t.id AND a.repo_id=r.id JOIN effective_team_memberships tm ON tm.team_id=t.id AND tm.user_id=$2 JOIN org_memberships m ON m.org_id=t.org_id AND m.user_id=tm.user_id JOIN users u ON u.id=tm.user_id WHERE r.id=$1 AND t.id=ANY($3) AND t.privacy IN ('visible','secret') AND ({PERMISSION})>=3 AND tm.role IN ('member','maintainer') AND m.role IN ('owner','admin','member') AND NOT u.is_banned AND NOT u.must_change_password)")).bind(repo).bind(actor).bind(ids).fetch_one(pool).await?)
}
