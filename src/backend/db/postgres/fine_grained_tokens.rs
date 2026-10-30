use crate::backend::db::{
    fine_grained_tokens::{Bounds, NewToken, Policy, OwnerTokens, Token, TokenFilter},
    models::OrgRole,
    organizations::OrganizationOutcome,
    credentials::Actor,
};
use anyhow::Result;
use chrono::{DateTime, Utc};
use sqlx::{PgConnection, PgPool};

/// A token's bounds with its live membership and policy; package transactions read it too.
pub(crate) const BOUNDS: &str = "SELECT f.token_id,t.user_id,f.owner_org_id,f.repository_access,f.permissions,f.status,t.expires_at,f.requested_at,(f.owner_org_id IS NULL OR EXISTS(SELECT 1 FROM org_memberships m WHERE m.org_id=f.owner_org_id AND m.user_id=t.user_id)) AS member,COALESCE(p.fine_grained_allowed,TRUE) AS fine_grained_allowed,CASE WHEN p.org_id IS NULL THEN 366 ELSE p.max_lifetime_days END AS max_lifetime_days FROM fine_grained_tokens f JOIN personal_access_tokens t ON t.id=f.token_id LEFT JOIN organization_token_policies p ON p.org_id=f.owner_org_id WHERE f.token_id=$1";
const TOKEN: &str = "SELECT f.token_id,t.user_id,t.name,f.description,f.owner_org_id,f.repository_access,f.permissions,f.status,f.reason,f.reviewed_at,f.requested_at,t.expires_at,t.created_at,t.last_used_at,t.last_used_ip FROM fine_grained_tokens f JOIN personal_access_tokens t ON t.id=f.token_id";

pub async fn create(
    pool: &PgPool,
    user: i64,
    session: &str,
    hash: &str,
    token: &NewToken<'_>,
) -> Result<Option<(i64, String)>> {
    let mut tx = pool.begin().await?;
    if !super::users::transitions::active_session(&mut tx, user, session).await? {
        return Ok(None);
    }
    // A token for an organization that is being deleted is refused.
    let organization_live = match token.owner_org_id {
        Some(org) => sqlx::query_scalar::<_, i64>("SELECT id FROM organizations WHERE id=$1 FOR SHARE")
            .bind(org).fetch_optional(&mut *tx).await?.is_some(),
        None => true,
    };
    if !organization_live {
        return Ok(None);
    }
    let status = match token.owner_org_id {
        None => "approved",
        Some(org) => {
            let row: Option<(String, bool, bool)> = sqlx::query_as("SELECT m.role,COALESCE(p.fine_grained_allowed,TRUE),COALESCE(p.fine_grained_approval,TRUE) FROM org_memberships m LEFT JOIN organization_token_policies p ON p.org_id=m.org_id WHERE m.org_id=$1 AND m.user_id=$2 FOR SHARE OF m")
                .bind(org).bind(user).fetch_optional(&mut *tx).await?;
            match row {
                Some((_, false, _)) | None => return Ok(None),
                Some((role, true, approval)) if approval && role != "owner" => "pending",
                Some(_) => "approved",
            }
        }
    };
    let id: i64 = sqlx::query_scalar("INSERT INTO personal_access_tokens(user_id,name,token_hash,scope,expires_at) VALUES($1,$2,$3,$4,$5) RETURNING id")
        .bind(user).bind(token.name).bind(hash).bind(token.scope).bind(token.expires_at).fetch_one(&mut *tx).await?;
    sqlx::query("INSERT INTO fine_grained_tokens(token_id,owner_org_id,description,repository_access,permissions,status) VALUES($1,$2,$3,$4,$5,$6)")
        .bind(id).bind(token.owner_org_id).bind(token.description).bind(token.repository_access).bind(token.permissions).bind(status).execute(&mut *tx).await?;
    for repo in token.repositories.iter().copied().collect::<std::collections::BTreeSet<_>>() {
        let inserted = sqlx::query("INSERT INTO fine_grained_token_repositories(token_id,repo_id) SELECT $1,id FROM repositories WHERE id=$2 AND NOT is_gist AND (($3::BIGINT IS NULL AND owner_kind='user' AND owner_user_id=$4) OR (owner_kind='org' AND owner_org_id=$3)) ON CONFLICT DO NOTHING")
            .bind(id).bind(repo).bind(token.owner_org_id).bind(user).execute(&mut *tx).await?.rows_affected();
        anyhow::ensure!(inserted == 1, "Selected repository changed during token creation");
    }
    tx.commit().await?;
    Ok(Some((id, status.to_owned())))
}

pub async fn regenerate(
    pool: &PgPool,
    user: i64,
    session: &str,
    token: i64,
    hash: &str,
    expires_at: Option<DateTime<Utc>>,
) -> Result<bool> {
    let mut tx = pool.begin().await?;
    if !super::users::transitions::active_session(&mut tx, user, session).await? { return Ok(false); }
    let changed = sqlx::query("UPDATE personal_access_tokens SET token_hash=$3,expires_at=$4,last_used_at=NULL,last_used_ip=NULL WHERE id=$1 AND user_id=$2 AND EXISTS(SELECT 1 FROM fine_grained_tokens WHERE token_id=$1)")
        .bind(token).bind(user).bind(hash).bind(expires_at).execute(&mut *tx).await?.rows_affected() == 1;
    if changed {
        sqlx::query(
            "UPDATE fine_grained_tokens SET requested_at=clock_timestamp() WHERE token_id=$1",
        )
        .bind(token)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(changed)
}

pub async fn repository_choices(pool: &PgPool, actor: i64, credential: &crate::backend::db::users::accounts::AccountCredential<'_>, org: Option<i64>, query: &str, selected: &[i64]) -> Result<crate::backend::db::fine_grained_tokens::RepositoryChoices> {
    anyhow::ensure!(selected.len() <= crate::backend::db::fine_grained_tokens::MAX_REPOSITORIES, "Repository selection exceeds its limit");
    let mut tx = pool.begin().await?;
    super::users::transitions::require_account(&mut tx, actor, credential).await?;
    let access = super::repositories::access::USER_REPO_ACCESS_SQL;
    let projection = format!("SELECT r.id,r.name FROM repositories r JOIN users u ON u.id=$1 WHERE NOT r.is_gist AND (($2::BIGINT IS NULL AND r.owner_kind='user' AND r.owner_user_id=u.id) OR ($2::BIGINT IS NOT NULL AND r.owner_kind='org' AND r.owner_org_id=$2)) AND ({access})>=1");
    let kept = sqlx::query_as(&format!("{projection} AND r.id=ANY($3) ORDER BY r.id"))
        .bind(actor).bind(org).bind(selected).fetch_all(&mut *tx).await?;
    let matching = sqlx::query_as(&format!("{projection} AND LOWER(r.name) LIKE $3 ESCAPE '\\' ORDER BY r.name,r.id LIMIT 101"))
        .bind(actor).bind(org).bind(crate::backend::db::discussions::contains_pattern(query)).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(crate::backend::db::fine_grained_tokens::RepositoryChoices { selected: kept, matching })
}

pub async fn get(pool: &PgPool, token: i64) -> Result<Option<Token>> {
    Ok(sqlx::query_as(&format!("{TOKEN} WHERE f.token_id=$1"))
        .bind(token)
        .fetch_optional(pool)
        .await?)
}

pub async fn list(pool: &PgPool, user: i64) -> Result<Vec<Token>> {
    list_before(pool, user, None).await
}

pub async fn list_before(pool: &PgPool, user: i64, before: Option<i64>) -> Result<Vec<Token>> {
    Ok(sqlx::query_as(&format!("{TOKEN} WHERE t.user_id=$1 AND (CAST($2 AS BIGINT) IS NULL OR f.token_id<$2) ORDER BY f.token_id DESC LIMIT 100"))
        .bind(user).bind(before).fetch_all(pool).await?)
}

pub async fn bounds(pool: &PgPool, token: i64) -> Result<Option<Bounds>> {
    Ok(sqlx::query_as(BOUNDS)
        .bind(token)
        .fetch_optional(pool)
        .await?)
}

pub async fn repositories(pool: &PgPool, token: i64, actor: i64, credential: &crate::backend::db::users::accounts::AccountCredential<'_>) -> Result<Vec<(i64, String)>> {
    use crate::backend::db::users::accounts::AccountCredential;
    let (session, presented) = match credential {
        AccountCredential::Session(session) => (Some(*session), None),
        AccountCredential::Token(id, hash) => (None, Some((*id, *hash))),
    };
    let issuer = crate::backend::db::oauth_apps::token_issuer_condition("t", "$6");
    let readable = super::repositories::access::readable_condition();
    Ok(sqlx::query_as(&format!("SELECT r.id,r.name FROM fine_grained_token_repositories selected
        JOIN fine_grained_tokens g ON g.token_id=selected.token_id JOIN personal_access_tokens p ON p.id=g.token_id
        JOIN repositories r ON r.id=selected.repo_id JOIN users u ON u.id=$2
        WHERE selected.token_id=$1 AND NOT u.is_banned AND NOT u.must_change_password AND NOT r.is_gist
        AND (p.user_id=u.id OR (g.owner_org_id IS NOT NULL AND EXISTS(
            SELECT 1 FROM org_memberships m WHERE m.org_id=g.owner_org_id AND m.user_id=u.id AND m.role='owner')))
        AND ((g.owner_org_id IS NULL AND r.owner_kind='user' AND r.owner_user_id=p.user_id)
             OR (g.owner_org_id IS NOT NULL AND r.owner_kind='org' AND r.owner_org_id=g.owner_org_id))
        AND (EXISTS(SELECT 1 FROM sessions s WHERE s.id=$3 AND s.user_id=u.id AND s.expires_at>clock_timestamp())
             OR EXISTS(SELECT 1 FROM personal_access_tokens t WHERE t.id=$4 AND t.token_hash=$5 AND t.user_id=u.id
                 AND (t.expires_at IS NULL OR t.expires_at>clock_timestamp()) AND {issuer}))
        AND {readable} ORDER BY r.name,r.id LIMIT 100"))
        .bind(token).bind(actor).bind(session).bind(presented.map(|(id, _)| id)).bind(presented.map(|(_, hash)| hash))
        .bind(crate::backend::identity::oauth_apps::CLI_CLIENT)
        .fetch_all(pool).await?)
}

pub async fn selected(pool: &PgPool, token: i64, repo: i64) -> Result<bool> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM fine_grained_token_repositories WHERE token_id=$1 AND repo_id=$2)",
    )
    .bind(token)
    .bind(repo)
    .fetch_one(pool)
    .await?)
}

pub async fn policy(pool: &PgPool, org: i64) -> Result<Policy> {
    Ok(sqlx::query_as("SELECT fine_grained_allowed,fine_grained_approval,classic_allowed,max_lifetime_days FROM organization_token_policies WHERE org_id=$1")
        .bind(org).fetch_optional(pool).await?.unwrap_or_default())
}

/// Policy and review writes need an active owner of an organization that still has one,
/// checked by the shared organization guard inside the writing transaction.
async fn active_owner(connection: &mut PgConnection, actor: i64, org: i64) -> Result<bool> {
    Ok(matches!(
        super::organizations::authority::authority(connection, actor, org).await?,
        OrganizationOutcome::Applied(OrgRole::Owner)
    ))
}

pub async fn set_policy(pool: &PgPool, actor: impl Into<Actor>, org: i64, policy: &Policy) -> Result<bool> {
    let owner = actor.into();
    let actor = owner.user_id;
    let mut tx = pool.begin().await?;
    if !active_owner(&mut tx, actor, org).await? || !super::credentials::organization(&mut tx, &owner, org, "organization_administration", true).await? {
        return Ok(false);
    }
    sqlx::query("INSERT INTO organization_token_policies(org_id,fine_grained_allowed,fine_grained_approval,classic_allowed,max_lifetime_days) VALUES($1,$2,$3,$4,$5) ON CONFLICT(org_id) DO UPDATE SET fine_grained_allowed=excluded.fine_grained_allowed,fine_grained_approval=excluded.fine_grained_approval,classic_allowed=excluded.classic_allowed,max_lifetime_days=excluded.max_lifetime_days")
        .bind(org).bind(policy.fine_grained_allowed).bind(policy.fine_grained_approval).bind(policy.classic_allowed).bind(policy.max_lifetime_days).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(true)
}

/// `organization_tokens` in the caller's connection.
async fn tokens_page(connection: &mut PgConnection, org: i64, filter: &TokenFilter<'_>, offset: i64, limit: i64) -> Result<Vec<Token>> {
    let order = if filter.ascending { "ASC" } else { "DESC" };
    let (permission, level) = filter.permission.map_or((None, None), |(key, level)| (Some(key), level));
    Ok(sqlx::query_as(&format!("{TOKEN} JOIN users u ON u.id=t.user_id WHERE f.owner_org_id=$1 AND f.status=$2 AND EXISTS(SELECT 1 FROM org_memberships m WHERE m.org_id=$1 AND m.user_id=t.user_id) AND (cardinality($3::text[])=0 OR u.username=ANY($3)) AND (cardinality($4::bigint[])=0 OR f.token_id=ANY($4)) AND ($5::bigint IS NULL OR f.repository_access='all' OR (f.repository_access='selected' AND EXISTS(SELECT 1 FROM fine_grained_token_repositories s WHERE s.token_id=f.token_id AND s.repo_id=$5))) AND ($6::text IS NULL OR ((f.permissions::jsonb->>$6) IS NOT NULL AND ($7::text IS NULL OR (f.permissions::jsonb->>$6)=$7))) AND ($8::timestamptz IS NULL OR t.last_used_at<$8) AND ($9::timestamptz IS NULL OR t.last_used_at>$9) AND ($12::bigint IS NULL OR f.token_id<$12) ORDER BY f.token_id {order} LIMIT $10 OFFSET $11"))
        .bind(org).bind(filter.status).bind(filter.owners).bind(filter.token_ids).bind(filter.repository).bind(permission).bind(level)
        .bind(filter.last_used_before).bind(filter.last_used_after).bind(limit).bind(offset).bind(filter.before)
        .fetch_all(connection).await?)
}

pub async fn organization_tokens(pool: &PgPool, org: i64, filter: &TokenFilter<'_>, offset: i64, limit: i64) -> Result<Vec<Token>> {
    tokens_page(&mut *pool.acquire().await?, org, filter, offset, limit).await
}

pub async fn owner_tokens(
    pool: &PgPool,
    actor: i64,
    session: &str,
    org: i64,
    before: (i64, i64),
    limit: i64,
) -> Result<Option<OwnerTokens>> {
    // One snapshot for the owner check and every list.
    let mut tx = pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
        .execute(&mut *tx)
        .await?;
    let owner: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM org_memberships m JOIN users u ON u.id=m.user_id JOIN sessions s ON s.user_id=u.id WHERE m.org_id=$1 AND m.user_id=$2 AND m.role='owner' AND NOT u.is_banned AND NOT u.must_change_password AND s.id=$3 AND s.expires_at>clock_timestamp())")
        .bind(org).bind(actor).bind(session).fetch_one(&mut *tx).await?;
    if !owner {
        return Ok(None);
    }
    let policy: Policy = sqlx::query_as("SELECT fine_grained_allowed,fine_grained_approval,classic_allowed,max_lifetime_days FROM organization_token_policies WHERE org_id=$1")
        .bind(org).fetch_optional(&mut *tx).await?.unwrap_or_default();
    let page = |status, before: i64| TokenFilter { status, before: (before > 0).then_some(before), ..Default::default() };
    let pending = tokens_page(&mut tx, org, &page("pending", before.0), 0, limit).await?;
    let approved = tokens_page(&mut tx, org, &page("approved", before.1), 0, limit).await?;
    let ids: Vec<i64> = pending.iter().chain(&approved).filter(|token| token.repository_access == "selected").map(|token| token.token_id).collect();
    let readable = super::repositories::access::readable_condition();
    let repositories: Vec<(i64, String)> = sqlx::query_as(&format!("SELECT token_id,name FROM (SELECT selected.token_id,r.name,ROW_NUMBER() OVER (PARTITION BY selected.token_id ORDER BY r.name,r.id) AS row_position
        FROM fine_grained_token_repositories selected JOIN fine_grained_tokens g ON g.token_id=selected.token_id
        JOIN repositories r ON r.id=selected.repo_id JOIN users u ON u.id=$2
        WHERE selected.token_id=ANY($1) AND g.owner_org_id=$3
        AND r.owner_kind='org' AND r.owner_org_id=$3 AND {readable}) listed WHERE row_position<=100 ORDER BY token_id,row_position"))
        .bind(ids.as_slice()).bind(actor).bind(org).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(Some(OwnerTokens { policy, pending, approved, repositories }))
}

pub async fn review(
    pool: &PgPool,
    org: i64,
    reviewer: impl Into<Actor>,
    tokens: &[i64],
    from: &str,
    status: &str,
    reason: Option<&str>,
) -> Result<u64> {
    let owner = reviewer.into();
    let reviewer = owner.user_id;
    let mut tx = pool.begin().await?;
    if !active_owner(&mut tx, reviewer, org).await? || !super::credentials::organization(&mut tx, &owner, org, "organization_administration", true).await? {
        return Ok(0);
    }
    let mut changed = 0;
    for token in tokens {
        changed += sqlx::query("UPDATE fine_grained_tokens SET status=$4,reason=$5,reviewed_by=$3,reviewed_at=clock_timestamp() WHERE token_id=$1 AND owner_org_id=$2 AND status=$6")
            .bind(token).bind(org).bind(reviewer).bind(status).bind(reason).bind(from).execute(&mut *tx).await?.rows_affected();
    }
    tx.commit().await?;
    Ok(changed)
}

/// The repository authority is retained by the caller before credential admission.
/// Lock the scoped grant before reading its current approval, selection and policy.
pub(crate) async fn repository_permission(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    token: i64,
    actor: i64,
    repo: i64,
    permission: &str,
    write: bool,
) -> Result<bool> {
    let _: Option<i64> = sqlx::query_scalar("SELECT token_id FROM fine_grained_tokens WHERE token_id=$1 FOR SHARE")
        .bind(token).fetch_optional(&mut **tx).await?;
    let bounds: Option<Bounds> = sqlx::query_as(BOUNDS).bind(token).fetch_optional(&mut **tx).await?;
    let Some(bounds) = bounds else {
        let allowed = crate::backend::db::fine_grained_tokens::classic_allowed_condition("$1", "r.owner_org_id");
        return Ok(sqlx::query_scalar::<_, bool>(&format!("SELECT NOT EXISTS(SELECT 1 FROM repositories r WHERE r.id=$2 AND NOT ({allowed}))"))
            .bind(token).bind(repo).fetch_one(&mut **tx).await?);
    };
    let permissions: serde_json::Map<String, serde_json::Value> = serde_json::from_str(&bounds.permissions)?;
    let scoped: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM repositories r WHERE r.id=$1 AND NOT r.is_gist AND (($2::bigint IS NULL AND r.owner_kind='user' AND r.owner_user_id=$3) OR (r.owner_kind='org' AND r.owner_org_id=$2)) AND ($4='all' OR EXISTS(SELECT 1 FROM fine_grained_token_repositories s WHERE s.token_id=$5 AND s.repo_id=r.id)))")
        .bind(repo).bind(bounds.owner_org_id).bind(actor).bind(&bounds.repository_access).bind(token).fetch_one(&mut **tx).await?;
    Ok(bounds.user_id == actor && bounds.active() && scoped && crate::backend::identity::fine_grained::allows(&permissions, permission, write))
}

pub async fn review_all(pool: &PgPool, org: i64, reviewer: impl Into<Actor>, from: &str, status: &str, reason: Option<&str>) -> Result<u64> {
    let owner = reviewer.into();
    let reviewer = owner.user_id;
    let mut tx = pool.begin().await?;
    if !active_owner(&mut tx, reviewer, org).await? || !super::credentials::organization(&mut tx, &owner, org, "organization_administration", true).await? {
        return Ok(0);
    }
    let changed = sqlx::query("UPDATE fine_grained_tokens SET status=$3,reason=$4,reviewed_by=$2,reviewed_at=clock_timestamp() WHERE owner_org_id=$1 AND status=$5 AND EXISTS(SELECT 1 FROM personal_access_tokens t JOIN org_memberships m ON m.user_id=t.user_id WHERE t.id=fine_grained_tokens.token_id AND m.org_id=$1)")
        .bind(org).bind(reviewer).bind(status).bind(reason).bind(from).execute(&mut *tx).await?.rows_affected();
    tx.commit().await?;
    Ok(changed)
}

pub async fn classic_refused(pool: &PgPool, token: i64, org: i64) -> Result<bool> {
    let allowed = crate::backend::db::fine_grained_tokens::classic_allowed_condition("$1", "$2");
    Ok(sqlx::query_scalar(&format!("SELECT NOT ({allowed})"))
        .bind(token).bind(org).fetch_one(pool).await?)
}

pub async fn classic_refused_repository(pool: &PgPool, token: i64, repo: i64) -> Result<bool> {
    let allowed = crate::backend::db::fine_grained_tokens::classic_allowed_condition("$1", "r.owner_org_id");
    Ok(sqlx::query_scalar(&format!("SELECT EXISTS(SELECT 1 FROM repositories r WHERE r.id=$2 AND NOT ({allowed}))"))
        .bind(token).bind(repo).fetch_one(pool).await?)
}
