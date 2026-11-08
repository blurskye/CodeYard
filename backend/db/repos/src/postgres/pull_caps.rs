use crate::pull_caps::CapUpdate;
use crate::pull_caps::PullCap;
use crate::pull_caps::PullCapReached;
use crate::pull_caps::PullCapRefusal;
use crate::pull_caps::PullCapScope;
use anyhow::Result;
use codeyard_database::models::OrgRole;
use codeyard_database::models::User;
use codeyard_database::pool::PgPool;
use codeyard_database::queries::organizations::OrganizationOutcome;
use codeyard_database::queries::users::accounts::AccountCredential;
use sqlx::PgConnection;

async fn authorize(
    connection: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    actor: i64,
    scope: PullCapScope,
    maintain: bool,
) -> Result<()> {
    if let PullCapScope::Repository(repo) = scope {
        let ordinary: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM repositories WHERE id=$1 AND NOT is_gist)",
        )
        .bind(repo)
        .fetch_one(&mut **connection)
        .await?;
        anyhow::ensure!(ordinary, PullCapRefusal::Denied);
    }
    let allowed = match scope {
        PullCapScope::Organization(org) => matches!(
            codeyard_database::queries::postgres::organizations::authority::authority(
                &mut *connection,
                actor,
                org
            )
            .await?,
            OrganizationOutcome::Applied(OrgRole::Owner | OrgRole::Admin)
        ),
        PullCapScope::Repository(repo) => {
            codeyard_database::queries::postgres::discussions::lock_policy_authority_with(
                connection,
                actor,
                repo,
                codeyard_database::queries::postgres::repositories::access::RepositoryRowLock::Exclusive,
                maintain,
            )
            .await?
        }
    };
    anyhow::ensure!(allowed, PullCapRefusal::Denied);
    Ok(())
}
async fn cap(connection: &mut PgConnection, scope: PullCapScope) -> Result<PullCap> {
    Ok(sqlx::query_as(sqlx::AssertSqlSafe(format!("SELECT enabled,max_open_pull_requests,include_drafts FROM interaction_pull_caps WHERE {}=$1",scope.column()))).bind(scope.id()).fetch_optional(connection).await?.unwrap_or_default())
}
pub async fn get_cap(pool: &PgPool, actor: i64, scope: PullCapScope) -> Result<PullCap> {
    let mut tx = pool.begin().await?;
    codeyard_database::queries::postgres::users::transitions::lock_accounts_shared(
        &mut tx,
        &[actor],
    )
    .await?;
    authorize(&mut tx, actor, scope, false).await?;
    let result = cap(&mut tx, scope).await?;
    tx.commit().await?;
    Ok(result)
}
pub async fn update_cap(
    pool: &PgPool,
    actor: i64,
    credential: &AccountCredential<'_>,
    scope: PullCapScope,
    input: &CapUpdate,
) -> Result<PullCap> {
    input.validate()?;
    let mut tx = pool.begin().await?;
    codeyard_database::queries::postgres::users::transitions::lock_accounts_shared(
        &mut tx,
        &[actor],
    )
    .await?;
    authorize(&mut tx, actor, scope, false).await?;
    codeyard_db_identity::postgres::users::transitions::require_account(&mut tx, actor, credential)
        .await?;
    let result = input.apply(cap(&mut tx, scope).await?);
    sqlx::query(sqlx::AssertSqlSafe(format!("INSERT INTO interaction_pull_caps({},enabled,max_open_pull_requests,include_drafts) VALUES($1,$2,$3,$4) ON CONFLICT({}) DO UPDATE SET enabled=excluded.enabled,max_open_pull_requests=excluded.max_open_pull_requests,include_drafts=excluded.include_drafts",scope.column(),scope.column()))).bind(scope.id()).bind(result.enabled).bind(result.max_open_pull_requests).bind(result.include_drafts).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(result)
}
pub async fn bypass_users(pool: &PgPool, actor: i64, repo: i64) -> Result<Vec<User>> {
    let mut tx = pool.begin().await?;
    codeyard_database::queries::postgres::users::transitions::lock_accounts_shared(
        &mut tx,
        &[actor],
    )
    .await?;
    authorize(&mut tx, actor, PullCapScope::Repository(repo), true).await?;
    let result = sqlx::query_as("SELECT u.* FROM users u JOIN interaction_pull_bypass b ON b.user_id=u.id WHERE b.repo_id=$1 ORDER BY LOWER(u.username),u.id").bind(repo).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(result)
}
pub async fn change_bypass(
    pool: &PgPool,
    actor: i64,
    credential: &AccountCredential<'_>,
    repo: i64,
    names: &[String],
    adding: bool,
) -> Result<()> {
    anyhow::ensure!(names.len() <= 100, PullCapRefusal::Limit);
    let mut tx = pool.begin().await?;
    codeyard_database::queries::postgres::users::transitions::lock_accounts_shared(
        &mut tx,
        &[actor],
    )
    .await?;
    authorize(&mut tx, actor, PullCapScope::Repository(repo), true).await?;
    codeyard_db_identity::postgres::users::transitions::require_account(&mut tx, actor, credential)
        .await?;
    for name in names {
        let id: i64 = sqlx::query_scalar(
            "SELECT id FROM users WHERE username=$1 AND (NOT is_banned OR NOT $2)",
        )
        .bind(name)
        .bind(adding)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(PullCapRefusal::MissingUser)?;
        if adding {
            sqlx::query("INSERT INTO interaction_pull_bypass(repo_id,user_id) VALUES($1,$2) ON CONFLICT DO NOTHING").bind(repo).bind(id).execute(&mut *tx).await?;
        } else {
            sqlx::query("DELETE FROM interaction_pull_bypass WHERE repo_id=$1 AND user_id=$2")
                .bind(repo)
                .bind(id)
                .execute(&mut *tx)
                .await?;
        }
    }
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM interaction_pull_bypass WHERE repo_id=$1")
            .bind(repo)
            .fetch_one(&mut *tx)
            .await?;
    anyhow::ensure!(count <= 100, PullCapRefusal::Limit);
    tx.commit().await?;
    Ok(())
}

/// All quota-increasing transitions lock the original author's counter before repository rows.
/// Organization and repository rows retain live policy and bypass authority.
pub(crate) async fn lock_author(connection: &mut PgConnection, author: i64) -> Result<()> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended(COALESCE(current_schema(),'')||'/pull-cap/'||$1::bigint::text,0))").bind(author).execute(connection).await?;
    Ok(())
}
/// A draft leaving draft state already counts toward caps that include drafts, so
/// `leaving_draft` checks only the caps that exclude them.
pub async fn check(
    connection: &mut PgConnection,
    author: i64,
    repo: i64,
    draft: bool,
    leaving_draft: bool,
    excluding: Option<i64>,
) -> Result<()> {
    let bypass: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM interaction_pull_bypass WHERE repo_id=$1 AND user_id=$2)",
    )
    .bind(repo)
    .bind(author)
    .fetch_one(&mut *connection)
    .await?;
    if bypass {
        return Ok(());
    }
    let caps: Vec<(Option<i64>,Option<i64>,i32,bool)> = sqlx::query_as("SELECT c.repo_id,c.org_id,c.max_open_pull_requests,c.include_drafts FROM interaction_pull_caps c JOIN repositories r ON r.id=$1 WHERE c.enabled AND (c.repo_id=r.id OR (NOT r.is_private AND r.owner_kind='org' AND c.org_id=r.owner_org_id)) AND (NOT $2 OR c.include_drafts) AND (NOT $3 OR NOT c.include_drafts)").bind(repo).bind(draft).bind(leaving_draft).fetch_all(&mut *connection).await?;
    for (repository, organization, maximum, drafts) in caps {
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pull_requests p JOIN repositories r ON r.id=p.repo_id WHERE p.author_id=$1 AND p.state='open' AND ($2 OR NOT p.is_draft) AND ($3::bigint IS NULL OR p.id<>$3) AND (p.repo_id=$4 OR (NOT r.is_private AND r.owner_kind='org' AND r.owner_org_id=$5))").bind(author).bind(drafts).bind(excluding).bind(repository).bind(organization).fetch_one(&mut *connection).await?;
        anyhow::ensure!(
            count < i64::from(maximum),
            PullCapReached {
                scope: if repository.is_some() {
                    "repository"
                } else {
                    "organization"
                },
                maximum
            }
        );
    }
    Ok(())
}
