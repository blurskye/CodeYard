use crate::fine_grained_tokens::NewToken;
use crate::fine_grained_tokens::OwnerTokens;
use crate::fine_grained_tokens::TokenFilter;
use anyhow::Result;
use chrono::DateTime;
use chrono::Utc;
use codeyard_db_core::models::OrgRole;
use codeyard_db_core::queries::credentials::Actor;
use codeyard_db_core::queries::fine_grained_tokens::Policy;
use codeyard_db_core::queries::fine_grained_tokens::Token;
use codeyard_db_core::queries::organizations::OrganizationOutcome;
use codeyard_db_core::queries::sqlite::fine_grained_tokens::policy;
use codeyard_db_core::queries::sqlite::fine_grained_tokens::repositories;
use codeyard_db_core::queries::sqlite::fine_grained_tokens::selected;
use sqlx::SqliteConnection;
use sqlx::SqlitePool;

const TOKEN: &str = "SELECT f.token_id,t.user_id,t.name,f.description,f.owner_org_id,f.repository_access,f.permissions,f.status,f.reason,f.reviewed_at,f.requested_at,t.expires_at,t.created_at,t.last_used_at,t.last_used_ip FROM fine_grained_tokens f JOIN personal_access_tokens t ON t.id=f.token_id";
const NOW: &str = "strftime('%Y-%m-%dT%H:%M:%f000Z','now')";

pub async fn create(
    pool: &SqlitePool,
    user: i64,
    session: &str,
    hash: &str,
    token: &NewToken<'_>,
) -> Result<Option<(i64, String)>> {
    // The write lock serializes with suspension, password resets and membership changes.
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    if !crate::sqlite::users::transitions::active_session(&mut tx, user, session).await? {
        return Ok(None);
    }
    let status = match token.owner_org_id {
        None => "approved",
        Some(org) => {
            let row: Option<(String, bool, bool)> = sqlx::query_as("SELECT m.role,COALESCE(p.fine_grained_allowed,1),COALESCE(p.fine_grained_approval,1) FROM org_memberships m LEFT JOIN organization_token_policies p ON p.org_id=m.org_id WHERE m.org_id=$1 AND m.user_id=$2")
                .bind(org).bind(user).fetch_optional(&mut *tx).await?;
            match row {
                Some((_, false, _)) | None => return Ok(None),
                Some((role, true, approval)) if approval && role != "owner" => "pending",
                Some(_) => "approved",
            }
        }
    };
    let id: i64 = sqlx::query_scalar("INSERT INTO personal_access_tokens(user_id,name,token_hash,scope,expires_at) VALUES($1,$2,$3,$4,$5) RETURNING id")
        .bind(user).bind(token.name).bind(hash).bind(token.scope).bind(token.expires_at.map(codeyard_db_core::queries::sqlite::time::encode)).fetch_one(&mut *tx).await?;
    sqlx::query("INSERT INTO fine_grained_tokens(token_id,owner_org_id,description,repository_access,permissions,status) VALUES($1,$2,$3,$4,$5,$6)")
        .bind(id).bind(token.owner_org_id).bind(token.description).bind(token.repository_access).bind(token.permissions).bind(status).execute(&mut *tx).await?;
    for repo in token
        .repositories
        .iter()
        .copied()
        .collect::<std::collections::BTreeSet<_>>()
    {
        let inserted = sqlx::query("INSERT OR IGNORE INTO fine_grained_token_repositories(token_id,repo_id) SELECT $1,id FROM repositories WHERE id=$2 AND NOT is_gist AND (($3 IS NULL AND owner_kind='user' AND owner_user_id=$4) OR (owner_kind='org' AND owner_org_id=$3))")
            .bind(id).bind(repo).bind(token.owner_org_id).bind(user).execute(&mut *tx).await?.rows_affected();
        anyhow::ensure!(
            inserted == 1,
            "Selected repository changed during token creation"
        );
    }
    tx.commit().await?;
    Ok(Some((id, status.to_owned())))
}

pub async fn regenerate(
    pool: &SqlitePool,
    user: i64,
    session: &str,
    token: i64,
    hash: &str,
    expires_at: Option<DateTime<Utc>>,
) -> Result<bool> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    if !crate::sqlite::users::transitions::active_session(&mut tx, user, session).await? {
        return Ok(false);
    }
    let changed = sqlx::query("UPDATE personal_access_tokens SET token_hash=$3,expires_at=$4,last_used_at=NULL,last_used_ip=NULL WHERE id=$1 AND user_id=$2 AND EXISTS(SELECT 1 FROM fine_grained_tokens WHERE token_id=$1)")
        .bind(token).bind(user).bind(hash).bind(expires_at.map(codeyard_db_core::queries::sqlite::time::encode)).execute(&mut *tx).await?.rows_affected() == 1;
    if changed {
        sqlx::query(&format!(
            "UPDATE fine_grained_tokens SET requested_at={NOW} WHERE token_id=$1"
        ))
        .bind(token)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(changed)
}

pub async fn repository_choices(
    pool: &SqlitePool,
    actor: i64,
    credential: &codeyard_db_core::queries::users::accounts::AccountCredential<'_>,
    org: Option<i64>,
    query: &str,
    selected: &[i64],
) -> Result<crate::fine_grained_tokens::RepositoryChoices> {
    anyhow::ensure!(
        selected.len() <= crate::fine_grained_tokens::MAX_REPOSITORIES,
        "Repository selection exceeds its limit"
    );
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    crate::sqlite::users::transitions::require_account(&mut tx, actor, credential).await?;
    let access = codeyard_db_core::queries::sqlite::repositories::access::USER_REPO_ACCESS_SQL;
    let projection = format!("SELECT r.id,r.name FROM repositories r JOIN users u ON u.id=?1 WHERE NOT r.is_gist AND ((?2 IS NULL AND r.owner_kind='user' AND r.owner_user_id=u.id) OR (?2 IS NOT NULL AND r.owner_kind='org' AND r.owner_org_id=?2)) AND ({access})>=1");
    let kept = sqlx::query_as(&format!(
        "{projection} AND r.id IN (SELECT value FROM json_each(?3)) ORDER BY r.id"
    ))
    .bind(actor)
    .bind(org)
    .bind(serde_json::to_string(selected)?)
    .fetch_all(&mut *tx)
    .await?;
    let matching = sqlx::query_as(&format!(
        "{projection} AND LOWER(r.name) LIKE ?3 ESCAPE '\\' ORDER BY r.name,r.id LIMIT 101"
    ))
    .bind(actor)
    .bind(org)
    .bind(codeyard_db_core::queries::discussions::contains_pattern(
        query,
    ))
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(crate::fine_grained_tokens::RepositoryChoices {
        selected: kept,
        matching,
    })
}

pub async fn get(pool: &SqlitePool, token: i64) -> Result<Option<Token>> {
    Ok(sqlx::query_as(&format!("{TOKEN} WHERE f.token_id=$1"))
        .bind(token)
        .fetch_optional(pool)
        .await?)
}

pub async fn list_before(pool: &SqlitePool, user: i64, before: Option<i64>) -> Result<Vec<Token>> {
    Ok(sqlx::query_as(&format!("{TOKEN} WHERE t.user_id=$1 AND (CAST($2 AS BIGINT) IS NULL OR f.token_id<$2) ORDER BY f.token_id DESC LIMIT 100"))
        .bind(user).bind(before).fetch_all(pool).await?)
}

/// Policy and review writes need an active owner of an organization that still has one,
/// checked by the shared organization guard inside the writing transaction.
async fn active_owner(connection: &mut SqliteConnection, actor: i64, org: i64) -> Result<bool> {
    Ok(matches!(
        codeyard_db_core::queries::sqlite::organizations::authority::authority(
            connection, actor, org
        )
        .await?,
        OrganizationOutcome::Applied(OrgRole::Owner)
    ))
}

pub async fn set_policy(
    pool: &SqlitePool,
    actor: impl Into<Actor>,
    org: i64,
    policy: &Policy,
) -> Result<bool> {
    let owner = actor.into();
    let actor = owner.user_id;
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    if !active_owner(&mut tx, actor, org).await?
        || !codeyard_db_core::queries::sqlite::credentials::organization(
            &mut tx,
            &owner,
            org,
            "organization_administration",
            true,
        )
        .await?
    {
        return Ok(false);
    }
    sqlx::query("INSERT INTO organization_token_policies(org_id,fine_grained_allowed,fine_grained_approval,classic_allowed,max_lifetime_days) VALUES($1,$2,$3,$4,$5) ON CONFLICT(org_id) DO UPDATE SET fine_grained_allowed=excluded.fine_grained_allowed,fine_grained_approval=excluded.fine_grained_approval,classic_allowed=excluded.classic_allowed,max_lifetime_days=excluded.max_lifetime_days")
        .bind(org).bind(policy.fine_grained_allowed).bind(policy.fine_grained_approval).bind(policy.classic_allowed).bind(policy.max_lifetime_days).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(true)
}

/// `organization_tokens` in the caller's connection.
async fn tokens_page(
    connection: &mut SqliteConnection,
    org: i64,
    filter: &TokenFilter<'_>,
    offset: i64,
    limit: i64,
) -> Result<Vec<Token>> {
    let order = if filter.ascending { "ASC" } else { "DESC" };
    // Catalog permission names are letters and underscores, so the JSON path is plain.
    let (permission, level) = filter.permission.map_or((None, None), |(key, level)| {
        (Some(format!("$.\"{key}\"")), level)
    });
    Ok(sqlx::query_as(&format!("{TOKEN} JOIN users u ON u.id=t.user_id WHERE f.owner_org_id=$1 AND f.status=$2 AND EXISTS(SELECT 1 FROM org_memberships m WHERE m.org_id=$1 AND m.user_id=t.user_id) AND (json_array_length($3)=0 OR u.username IN (SELECT value FROM json_each($3))) AND (json_array_length($4)=0 OR f.token_id IN (SELECT value FROM json_each($4))) AND ($5 IS NULL OR f.repository_access='all' OR (f.repository_access='selected' AND EXISTS(SELECT 1 FROM fine_grained_token_repositories s WHERE s.token_id=f.token_id AND s.repo_id=$5))) AND ($6 IS NULL OR (json_extract(f.permissions,$6) IS NOT NULL AND ($7 IS NULL OR json_extract(f.permissions,$6)=$7))) AND ($8 IS NULL OR julianday(t.last_used_at)<julianday($8)) AND ($9 IS NULL OR julianday(t.last_used_at)>julianday($9)) AND ($12 IS NULL OR f.token_id<$12) ORDER BY f.token_id {order} LIMIT $10 OFFSET $11"))
        .bind(org).bind(filter.status).bind(serde_json::to_string(filter.owners)?).bind(serde_json::to_string(filter.token_ids)?).bind(filter.repository).bind(permission).bind(level)
        .bind(filter.last_used_before).bind(filter.last_used_after).bind(limit).bind(offset).bind(filter.before)
        .fetch_all(connection).await?)
}

pub async fn organization_tokens(
    pool: &SqlitePool,
    org: i64,
    filter: &TokenFilter<'_>,
    offset: i64,
    limit: i64,
) -> Result<Vec<Token>> {
    tokens_page(&mut *pool.acquire().await?, org, filter, offset, limit).await
}

pub async fn owner_tokens(
    pool: &SqlitePool,
    actor: i64,
    session: &str,
    org: i64,
    before: (i64, i64),
    limit: i64,
) -> Result<Option<OwnerTokens>> {
    // A deferred transaction reads every list from the snapshot that saw the owner.
    let mut tx = pool.begin().await?;
    let owner: bool = sqlx::query_scalar(&format!("SELECT EXISTS(SELECT 1 FROM org_memberships m JOIN users u ON u.id=m.user_id JOIN sessions s ON s.user_id=u.id WHERE m.org_id=$1 AND m.user_id=$2 AND m.role='owner' AND NOT u.is_banned AND NOT u.must_change_password AND s.id=$3 AND s.expires_at>{NOW})"))
        .bind(org).bind(actor).bind(session).fetch_one(&mut *tx).await?;
    if !owner {
        return Ok(None);
    }
    let policy: Policy = sqlx::query_as("SELECT fine_grained_allowed,fine_grained_approval,classic_allowed,max_lifetime_days FROM organization_token_policies WHERE org_id=$1")
        .bind(org).fetch_optional(&mut *tx).await?.unwrap_or_default();
    let page = |status, before: i64| TokenFilter {
        status,
        before: (before > 0).then_some(before),
        ..Default::default()
    };
    let pending = tokens_page(&mut tx, org, &page("pending", before.0), 0, limit).await?;
    let approved = tokens_page(&mut tx, org, &page("approved", before.1), 0, limit).await?;
    let ids: Vec<i64> = pending
        .iter()
        .chain(&approved)
        .filter(|token| token.repository_access == "selected")
        .map(|token| token.token_id)
        .collect();
    let readable = codeyard_db_core::queries::sqlite::repositories::access::readable_condition();
    let repositories: Vec<(i64, String)> = sqlx::query_as(&format!("SELECT token_id,name FROM (SELECT selected.token_id,r.name,ROW_NUMBER() OVER (PARTITION BY selected.token_id ORDER BY r.name,r.id) AS row_position
        FROM fine_grained_token_repositories selected JOIN fine_grained_tokens g ON g.token_id=selected.token_id
        JOIN repositories r ON r.id=selected.repo_id JOIN users u ON u.id=$2
        WHERE selected.token_id IN (SELECT value FROM json_each($1)) AND g.owner_org_id=$3
        AND r.owner_kind='org' AND r.owner_org_id=$3 AND {readable}) listed WHERE row_position<=100 ORDER BY token_id,row_position"))
        .bind(serde_json::to_string(&ids)?).bind(actor).bind(org).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(Some(OwnerTokens {
        policy,
        pending,
        approved,
        repositories,
    }))
}

pub async fn review(
    pool: &SqlitePool,
    org: i64,
    reviewer: impl Into<Actor>,
    tokens: &[i64],
    from: &str,
    status: &str,
    reason: Option<&str>,
) -> Result<u64> {
    let owner = reviewer.into();
    let reviewer = owner.user_id;
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    if !active_owner(&mut tx, reviewer, org).await?
        || !codeyard_db_core::queries::sqlite::credentials::organization(
            &mut tx,
            &owner,
            org,
            "organization_administration",
            true,
        )
        .await?
    {
        return Ok(0);
    }
    let mut changed = 0;
    for token in tokens {
        changed += sqlx::query(&format!("UPDATE fine_grained_tokens SET status=$4,reason=$5,reviewed_by=$3,reviewed_at={NOW} WHERE token_id=$1 AND owner_org_id=$2 AND status=$6"))
            .bind(token).bind(org).bind(reviewer).bind(status).bind(reason).bind(from).execute(&mut *tx).await?.rows_affected();
    }
    tx.commit().await?;
    Ok(changed)
}

pub async fn review_all(
    pool: &SqlitePool,
    org: i64,
    reviewer: impl Into<Actor>,
    from: &str,
    status: &str,
    reason: Option<&str>,
) -> Result<u64> {
    let owner = reviewer.into();
    let reviewer = owner.user_id;
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    if !active_owner(&mut tx, reviewer, org).await?
        || !codeyard_db_core::queries::sqlite::credentials::organization(
            &mut tx,
            &owner,
            org,
            "organization_administration",
            true,
        )
        .await?
    {
        return Ok(0);
    }
    let changed = sqlx::query(&format!("UPDATE fine_grained_tokens SET status=$3,reason=$4,reviewed_by=$2,reviewed_at={NOW} WHERE owner_org_id=$1 AND status=$5 AND EXISTS(SELECT 1 FROM personal_access_tokens t JOIN org_memberships m ON m.user_id=t.user_id WHERE t.id=fine_grained_tokens.token_id AND m.org_id=$1)"))
        .bind(org).bind(reviewer).bind(status).bind(reason).bind(from).execute(&mut *tx).await?.rows_affected();
    tx.commit().await?;
    Ok(changed)
}

pub async fn classic_refused(pool: &SqlitePool, token: i64, org: i64) -> Result<bool> {
    let allowed =
        codeyard_db_core::queries::fine_grained_tokens::classic_allowed_condition("$1", "$2");
    Ok(sqlx::query_scalar(&format!("SELECT NOT ({allowed})"))
        .bind(token)
        .bind(org)
        .fetch_one(pool)
        .await?)
}

pub async fn classic_refused_repository(pool: &SqlitePool, token: i64, repo: i64) -> Result<bool> {
    let allowed = codeyard_db_core::queries::fine_grained_tokens::classic_allowed_condition(
        "$1",
        "r.owner_org_id",
    );
    Ok(sqlx::query_scalar(&format!(
        "SELECT EXISTS(SELECT 1 FROM repositories r WHERE r.id=$2 AND NOT ({allowed}))"
    ))
    .bind(token)
    .bind(repo)
    .fetch_one(pool)
    .await?)
}
