use crate::pool::SqlitePool;
use crate::queries::fine_grained_tokens::Bounds;
use crate::queries::fine_grained_tokens::Policy;
use anyhow::Result;
/// A token's bounds with its live membership and policy; package transactions read it too.
pub const BOUNDS: &str = "SELECT f.token_id,t.user_id,f.owner_org_id,f.repository_access,f.permissions,f.status,t.expires_at,f.requested_at,(f.owner_org_id IS NULL OR EXISTS(SELECT 1 FROM org_memberships m WHERE m.org_id=f.owner_org_id AND m.user_id=t.user_id)) AS member,COALESCE(p.fine_grained_allowed,1) AS fine_grained_allowed,CASE WHEN p.org_id IS NULL THEN 366 ELSE p.max_lifetime_days END AS max_lifetime_days FROM fine_grained_tokens f JOIN personal_access_tokens t ON t.id=f.token_id LEFT JOIN organization_token_policies p ON p.org_id=f.owner_org_id WHERE f.token_id=$1";

pub async fn bounds(pool: &SqlitePool, token: i64) -> Result<Option<Bounds>> {
    Ok(sqlx::query_as(BOUNDS)
        .bind(token)
        .fetch_optional(pool)
        .await?)
}

pub async fn repositories(
    pool: &SqlitePool,
    token: i64,
    actor: i64,
    credential: &crate::queries::users::accounts::AccountCredential<'_>,
) -> Result<Vec<(i64, String)>> {
    use crate::queries::users::accounts::AccountCredential;
    let (session, presented) = match credential {
        AccountCredential::Session(session) => (Some(*session), None),
        AccountCredential::Token(id, hash) => (None, Some((*id, *hash))),
        AccountCredential::ForeignToken(..) => return Ok(vec![]),
    };
    let issuer = crate::queries::oauth_apps::token_issuer_condition("t", "?6");
    let readable = crate::queries::sqlite::repositories::access::readable_condition();
    Ok(sqlx::query_as(&format!("SELECT r.id,r.name FROM fine_grained_token_repositories selected
        JOIN fine_grained_tokens g ON g.token_id=selected.token_id JOIN personal_access_tokens p ON p.id=g.token_id
        JOIN repositories r ON r.id=selected.repo_id JOIN users u ON u.id=?2
        WHERE selected.token_id=?1 AND NOT u.is_banned AND NOT u.must_change_password AND NOT r.is_gist
        AND (p.user_id=u.id OR (g.owner_org_id IS NOT NULL AND EXISTS(
            SELECT 1 FROM org_memberships m WHERE m.org_id=g.owner_org_id AND m.user_id=u.id AND m.role='owner')))
        AND ((g.owner_org_id IS NULL AND r.owner_kind='user' AND r.owner_user_id=p.user_id)
             OR (g.owner_org_id IS NOT NULL AND r.owner_kind='org' AND r.owner_org_id=g.owner_org_id))
        AND (EXISTS(SELECT 1 FROM sessions s WHERE s.id=?3 AND s.user_id=u.id AND s.expires_at>strftime('%Y-%m-%dT%H:%M:%f000Z','now'))
             OR EXISTS(SELECT 1 FROM personal_access_tokens t WHERE t.id=?4 AND t.token_hash=?5 AND t.user_id=u.id
                 AND (t.expires_at IS NULL OR t.expires_at>strftime('%Y-%m-%dT%H:%M:%f000Z','now')) AND {issuer}))
        AND {readable} ORDER BY r.name,r.id LIMIT 100"))
        .bind(token).bind(actor).bind(session).bind(presented.map(|(id, _)| id)).bind(presented.map(|(_, hash)| hash))
        .bind(codeyard_types::identity::CLI_CLIENT)
        .fetch_all(pool).await?)
}

pub async fn selected(pool: &SqlitePool, token: i64, repo: i64) -> Result<bool> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM fine_grained_token_repositories WHERE token_id=$1 AND repo_id=$2)",
    )
    .bind(token)
    .bind(repo)
    .fetch_one(pool)
    .await?)
}

pub async fn policy(pool: &SqlitePool, org: i64) -> Result<Policy> {
    Ok(sqlx::query_as("SELECT fine_grained_allowed,fine_grained_approval,classic_allowed,max_lifetime_days FROM organization_token_policies WHERE org_id=$1")
        .bind(org).fetch_optional(pool).await?.unwrap_or_default())
}

pub async fn repository_permission(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    token: i64,
    actor: i64,
    repo: i64,
    permission: &str,
    write: bool,
) -> Result<bool> {
    let bounds: Option<Bounds> = sqlx::query_as(BOUNDS)
        .bind(token)
        .fetch_optional(&mut **tx)
        .await?;
    let Some(bounds) = bounds else {
        let allowed =
            crate::queries::fine_grained_tokens::classic_allowed_condition("$1", "r.owner_org_id");
        return Ok(sqlx::query_scalar::<_, bool>(&format!(
            "SELECT NOT EXISTS(SELECT 1 FROM repositories r WHERE r.id=$2 AND NOT ({allowed}))"
        ))
        .bind(token)
        .bind(repo)
        .fetch_one(&mut **tx)
        .await?);
    };
    let permissions: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(&bounds.permissions)?;
    let scoped: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM repositories r WHERE r.id=$1 AND NOT r.is_gist AND (($2 IS NULL AND r.owner_kind='user' AND r.owner_user_id=$3) OR (r.owner_kind='org' AND r.owner_org_id=$2)) AND ($4='all' OR EXISTS(SELECT 1 FROM fine_grained_token_repositories s WHERE s.token_id=$5 AND s.repo_id=r.id)))")
        .bind(repo).bind(bounds.owner_org_id).bind(actor).bind(&bounds.repository_access).bind(token).fetch_one(&mut **tx).await?;
    Ok(bounds.user_id == actor
        && bounds.active()
        && scoped
        && codeyard_types::fine_grained::allows(&permissions, permission, write))
}
