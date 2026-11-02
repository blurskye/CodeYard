use crate::backend::db::{
    interactions::ContributorSource,
    interactions::{
        email_digest, InteractionRefusal, InteractionScope, NewRestriction, Restriction,
    },
    models::OrgRole,
    organizations::OrganizationOutcome,
    users::accounts::AccountCredential,
};
use anyhow::Result;
use sqlx::{SqliteConnection, SqlitePool};

const FIELDS: &str = "restriction AS \"limit\",CASE WHEN user_id IS NOT NULL THEN 'user' WHEN org_id IS NOT NULL THEN 'organization' ELSE 'repository' END AS origin,expires_at";
async fn effective(connection: &mut SqliteConnection, repo: i64) -> Result<Option<Restriction>> {
    Ok(sqlx::query_as(&format!("SELECT {FIELDS} FROM interaction_limits l WHERE expires_at>strftime('%Y-%m-%dT%H:%M:%f000Z','now') AND EXISTS(SELECT 1 FROM repositories r WHERE r.id=?1 AND (l.repo_id=r.id OR (NOT r.is_private AND ((r.owner_kind='user' AND l.user_id=r.owner_user_id) OR (r.owner_kind='org' AND l.org_id=r.owner_org_id))))) ORDER BY (repo_id IS NOT NULL) LIMIT 1")).bind(repo).fetch_optional(connection).await?)
}
pub async fn source(pool: &SqlitePool, repo: i64) -> Result<Option<ContributorSource>> {
    Ok(sqlx::query_as("SELECT id AS repo_id,default_branch,storage_node_id,contribution_version AS write_version FROM repositories WHERE id=?1").bind(repo).fetch_optional(pool).await?)
}
pub async fn effective_restriction(pool: &SqlitePool, repo: i64) -> Result<Option<Restriction>> {
    effective(&mut *pool.acquire().await?, repo).await
}
async fn scoped(
    connection: &mut SqliteConnection,
    scope: InteractionScope,
) -> Result<Option<Restriction>> {
    if let InteractionScope::Repository(repo) = scope {
        return effective(connection, repo).await;
    }
    Ok(sqlx::query_as(&format!("SELECT {FIELDS} FROM interaction_limits WHERE {}=?1 AND expires_at>strftime('%Y-%m-%dT%H:%M:%f000Z','now')",scope.column())).bind(scope.id()).fetch_optional(connection).await?)
}
async fn authorize(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    actor: i64,
    scope: InteractionScope,
    write: bool,
) -> Result<()> {
    if let InteractionScope::Repository(repo) = scope {
        let ordinary: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM repositories WHERE id=?1 AND NOT is_gist)",
        )
        .bind(repo)
        .fetch_one(&mut **tx)
        .await?;
        anyhow::ensure!(ordinary, InteractionRefusal::Denied);
    }
    let allowed = match scope {
        InteractionScope::User(user) => user==actor && sqlx::query_scalar::<_,bool>("SELECT EXISTS(SELECT 1 FROM users WHERE id=?1 AND NOT is_banned AND NOT must_change_password)").bind(actor).fetch_one(&mut **tx).await?,
        InteractionScope::Organization(org) => match super::organizations::authority::authority(tx,actor,org).await? {OrganizationOutcome::Applied(OrgRole::Owner)=>true,OrganizationOutcome::Applied(OrgRole::Admin)=>!write,_=>false},
        InteractionScope::Repository(repo) => super::discussions::lock_archive_authority(tx,actor,repo).await?,
    };
    anyhow::ensure!(allowed, InteractionRefusal::Denied);
    Ok(())
}
pub async fn get_restriction(
    pool: &SqlitePool,
    actor: i64,
    scope: InteractionScope,
) -> Result<Option<Restriction>> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    authorize(&mut tx, actor, scope, false).await?;
    let result = scoped(&mut tx, scope).await?;
    tx.commit().await?;
    Ok(result)
}
pub async fn set_restriction(
    pool: &SqlitePool,
    actor: i64,
    credential: &AccountCredential<'_>,
    scope: InteractionScope,
    input: Option<NewRestriction>,
) -> Result<Option<Restriction>> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    authorize(&mut tx, actor, scope, true).await?;
    super::users::transitions::require_account(&mut tx, actor, credential).await?;
    if let InteractionScope::Repository(repo) = scope {
        let overridden: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM interaction_limits l JOIN repositories r ON (r.owner_kind='user' AND r.owner_user_id=l.user_id) OR (r.owner_kind='org' AND r.owner_org_id=l.org_id) WHERE r.id=?1 AND NOT r.is_private AND l.repo_id IS NULL AND l.expires_at>strftime('%Y-%m-%dT%H:%M:%f000Z','now'))").bind(repo).fetch_one(&mut *tx).await?;
        anyhow::ensure!(!overridden, InteractionRefusal::Conflict);
    }
    sqlx::query(&format!(
        "DELETE FROM interaction_limits WHERE {}=?1",
        scope.column()
    ))
    .bind(scope.id())
    .execute(&mut *tx)
    .await?;
    if input.is_some() {
        match scope {
            InteractionScope::User(user) => {
                sqlx::query("DELETE FROM interaction_limits WHERE repo_id IN (SELECT id FROM repositories WHERE owner_kind='user' AND owner_user_id=?1 AND NOT is_private)").bind(user).execute(&mut *tx).await?;
            }
            InteractionScope::Organization(org) => {
                sqlx::query("DELETE FROM interaction_limits WHERE repo_id IN (SELECT id FROM repositories WHERE owner_kind='org' AND owner_org_id=?1 AND NOT is_private)").bind(org).execute(&mut *tx).await?;
            }
            InteractionScope::Repository(_) => (),
        }
    }
    if let Some(input) = input {
        sqlx::query(&format!("INSERT INTO interaction_limits({},restriction,expires_at) VALUES(?1,?2,strftime('%Y-%m-%dT%H:%M:%f000Z','now',?3))",scope.column())).bind(scope.id()).bind(input.limit.as_str()).bind(format!("+{} seconds",input.expiry.seconds())).execute(&mut *tx).await?;
    }
    let result = scoped(&mut tx, scope).await?;
    tx.commit().await?;
    Ok(result)
}
async fn emails(connection: &mut SqliteConnection, actor: i64) -> Result<Vec<String>> {
    Ok(sqlx::query_scalar("SELECT username||'@users.noreply' AS email FROM users WHERE id=?1 UNION SELECT email FROM users WHERE id=?1 AND email_verified AND email IS NOT NULL UNION SELECT email FROM user_emails WHERE user_id=?1 AND verified").bind(actor).fetch_all(connection).await?)
}
pub async fn verified_emails(pool: &SqlitePool, actor: i64) -> Result<Vec<String>> {
    emails(&mut *pool.acquire().await?, actor).await
}
async fn proof(
    connection: &mut SqliteConnection,
    actor: i64,
    source: &ContributorSource,
    emails: &[String],
) -> Result<Option<bool>> {
    Ok(sqlx::query_scalar("SELECT p.contributed FROM interaction_contributors p JOIN repositories r ON r.id=p.repo_id WHERE p.repo_id=?1 AND p.user_id=?2 AND p.write_version=?3 AND p.default_branch=?4 AND p.storage_node_id IS ?5 AND p.emails_hash=?6 AND r.contribution_version=p.write_version AND r.default_branch=p.default_branch AND r.storage_node_id IS p.storage_node_id")
        .bind(source.repo_id).bind(actor).bind(source.write_version).bind(&source.default_branch).bind(source.storage_node_id).bind(email_digest(emails)).fetch_optional(connection).await?)
}
pub async fn proof_current(
    pool: &SqlitePool,
    actor: i64,
    source: &ContributorSource,
    emails: &[String],
) -> Result<Option<bool>> {
    proof(&mut *pool.acquire().await?, actor, source, emails).await
}
pub async fn save_proof(
    pool: &SqlitePool,
    actor: i64,
    source: &ContributorSource,
    addresses: &[String],
    contributed: bool,
) -> Result<bool> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let access = super::repositories::access::user_repo_access_in_transaction(
        &mut tx,
        actor,
        source.repo_id,
    )
    .await?;
    let admin: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM repositories r JOIN users u ON u.id=?2 WHERE r.id=?1 AND r.owner_kind='user' AND u.is_admin AND NOT u.is_banned AND NOT u.must_change_password)").bind(source.repo_id).bind(actor).fetch_one(&mut *tx).await?;
    if !access.can_read() && !admin {
        return Ok(false);
    }
    if email_digest(&emails(&mut tx, actor).await?) != email_digest(addresses) {
        return Ok(false);
    }
    let changed = sqlx::query("INSERT INTO interaction_contributors(repo_id,user_id,write_version,default_branch,storage_node_id,emails_hash,contributed) SELECT id,?2,contribution_version,default_branch,storage_node_id,?6,?7 FROM repositories WHERE id=?1 AND contribution_version=?3 AND default_branch=?4 AND storage_node_id IS ?5 ON CONFLICT(repo_id,user_id) DO UPDATE SET write_version=excluded.write_version,default_branch=excluded.default_branch,storage_node_id=excluded.storage_node_id,emails_hash=excluded.emails_hash,contributed=excluded.contributed")
        .bind(source.repo_id).bind(actor).bind(source.write_version).bind(&source.default_branch).bind(source.storage_node_id).bind(email_digest(addresses)).bind(contributed).execute(&mut *tx).await?.rows_affected()==1;
    tx.commit().await?;
    Ok(changed)
}

async fn contribution_exempt(
    connection: &mut SqliteConnection,
    actor: i64,
    repo: i64,
    restriction: &Restriction,
) -> Result<bool> {
    if restriction.origin == "organization" {
        let member: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM org_memberships m JOIN repositories r ON r.owner_kind='org' AND r.owner_org_id=m.org_id WHERE r.id=?1 AND m.user_id=?2 AND m.role IN ('owner','admin','member'))").bind(repo).bind(actor).fetch_one(&mut *connection).await?;
        if member {
            return Ok(true);
        }
    }
    if restriction.limit == "existing_users" {
        return Ok(sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM users WHERE id=?1 AND created_at<=strftime('%Y-%m-%dT%H:%M:%f000Z','now','-1 day'))").bind(actor).fetch_one(connection).await?);
    }
    Ok(false)
}

pub async fn proof_required(pool: &SqlitePool, actor: i64, repo: i64) -> Result<bool> {
    let mut connection = pool.acquire().await?;
    let Some(restriction) = effective(&mut connection, repo).await? else {
        return Ok(false);
    };
    Ok(restriction.limit != "collaborators_only"
        && !contribution_exempt(&mut connection, actor, repo, &restriction).await?)
}

/// BEGIN IMMEDIATE and the normal discussion authority keep this policy live through the effect.
pub(crate) async fn allows_contribution(
    connection: &mut SqliteConnection,
    actor: i64,
    repo: i64,
    collaborator: bool,
) -> Result<bool> {
    if collaborator {
        return Ok(true);
    }
    let Some(restriction) = effective(&mut *connection, repo).await? else {
        return Ok(true);
    };
    if contribution_exempt(connection, actor, repo, &restriction).await? {
        return Ok(true);
    }
    if restriction.limit == "collaborators_only" {
        return Ok(false);
    }
    let source: ContributorSource = sqlx::query_as("SELECT id AS repo_id,default_branch,storage_node_id,contribution_version AS write_version FROM repositories WHERE id=?1").bind(repo).fetch_one(&mut *connection).await?;
    let addresses = emails(&mut *connection, actor).await?;
    proof(connection, actor, &source, &addresses)
        .await?
        .ok_or_else(|| anyhow::anyhow!("Contributor proof changed or could not be loaded"))
}
