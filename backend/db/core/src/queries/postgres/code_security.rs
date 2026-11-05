use crate::models::OrgRole;
use crate::models::Repository;
use crate::pool::PgPool;
use crate::queries::code_security::apply;
use crate::queries::code_security::private_reporting;
use crate::queries::code_security::scanning_settings;
use crate::queries::code_security::Configuration;
use crate::queries::code_security::Refusal;
use crate::queries::code_security::RepositorySettings;
use crate::queries::organizations::OrganizationOutcome;
use anyhow::ensure;
use anyhow::Result;
use sqlx::PgConnection;
pub async fn readable(connection: &mut PgConnection, actor: i64, org: i64) -> Result<()> {
    match crate::queries::postgres::organizations::authority::read_authority(connection, actor, org)
        .await?
    {
        OrganizationOutcome::Applied(OrgRole::Owner | OrgRole::Admin) => Ok(()),
        OrganizationOutcome::Applied(OrgRole::Member)
            if crate::queries::postgres::organizations::roles::holds_security_role(
                connection, actor, org,
            )
            .await? =>
        {
            Ok(())
        }
        OrganizationOutcome::Missing => Err(Refusal::Missing.into()),
        _ => Err(Refusal::Denied.into()),
    }
}

pub async fn configuration(
    connection: &mut PgConnection,
    org: i64,
    id: i64,
) -> Result<Configuration> {
    Ok(
        sqlx::query_as("SELECT * FROM code_security_configurations WHERE org_id=$1 AND id=$2")
            .bind(org)
            .bind(id)
            .fetch_optional(connection)
            .await?
            .ok_or(Refusal::Missing)?,
    )
}

pub async fn apply_repository(
    connection: &mut PgConnection,
    repo: i64,
    actor: Option<i64>,
    config: Option<&Configuration>,
) -> Result<()> {
    let current: Option<RepositorySettings> =
        sqlx::query_as("SELECT * FROM repository_security_settings WHERE repo_id=$1")
            .bind(repo)
            .fetch_optional(&mut *connection)
            .await?;
    let settings = match config {
        Some(config) => apply(
            current
                .as_ref()
                .map(|row| row.security_and_analysis.as_str()),
            &config.settings,
        )?,
        None => current.map_or_else(|| "{}".into(), |row| row.security_and_analysis),
    };
    if let Some(config) = config {
        let previous: Option<crate::queries::secret_scanning::Settings> = sqlx::query_as(
            "SELECT enabled,push_protection FROM secret_scanning_settings WHERE repo_id=$1 FOR UPDATE"
        ).bind(repo).fetch_optional(&mut *connection).await?;
        crate::queries::postgres::secret_scanning::save_settings_on_connection(
            connection,
            actor,
            repo,
            &scanning_settings(
                &apply(None, &config.settings)?,
                previous.unwrap_or_default(),
            )?,
        )
        .await?;
        // Private vulnerability reporting lives in its own table and is offered on public
        // repositories only.
        match private_reporting(&config.settings, false)? {
            Some(true) => {
                sqlx::query("INSERT INTO repository_private_reporting(repo_id,enabled_by_id) SELECT id,$2 FROM repositories WHERE id=$1 AND NOT is_private ON CONFLICT(repo_id) DO NOTHING")
                    .bind(repo).bind(actor).execute(&mut *connection).await?;
            }
            Some(false) => {
                sqlx::query("DELETE FROM repository_private_reporting WHERE repo_id=$1")
                    .bind(repo)
                    .execute(&mut *connection)
                    .await?;
            }
            None => {}
        }
    }
    sqlx::query("INSERT INTO repository_security_settings(repo_id,configuration_id,security_and_analysis) VALUES($1,$2,$3) ON CONFLICT(repo_id) DO UPDATE SET configuration_id=excluded.configuration_id,security_and_analysis=excluded.security_and_analysis")
        .bind(repo).bind(config.map(|c|c.id)).bind(settings).execute(connection).await?;
    Ok(())
}

pub async fn repositories(
    pool: &PgPool,
    actor: i64,
    org: i64,
    id: i64,
    after: i64,
    before: i64,
    limit: i64,
) -> Result<Vec<Repository>> {
    ensure!(after >= 0 && (1..=101).contains(&limit), Refusal::Invalid);
    let mut tx = pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
        .execute(&mut *tx)
        .await?;
    readable(&mut tx, actor, org).await?;
    configuration(&mut tx, org, id).await?;
    let direction = if before < i64::MAX { "DESC" } else { "ASC" };
    let rows = sqlx::query_as(&format!(
        "SELECT r.* FROM repositories r JOIN repository_security_settings s ON s.repo_id=r.id WHERE r.owner_org_id=$1 AND s.configuration_id=$2 AND r.id>$3 AND r.id<$4 ORDER BY r.id {direction} LIMIT $5"
    ))
    .bind(org)
    .bind(id)
    .bind(after)
    .bind(before)
    .bind(limit)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(rows)
}

pub async fn repository_configuration<'e>(
    pool: impl sqlx::Executor<'e, Database = sqlx::Postgres>,
    repo: i64,
) -> Result<Option<Configuration>> {
    Ok(sqlx::query_as("SELECT c.* FROM code_security_configurations c JOIN repository_security_settings s ON s.configuration_id=c.id JOIN repositories r ON r.id=s.repo_id AND r.owner_org_id=c.org_id WHERE r.id=$1").bind(repo).fetch_optional(pool).await?)
}

pub async fn apply_default(connection: &mut PgConnection, repo: &Repository) -> Result<()> {
    let Some(org) = repo.owner_org_id else {
        return Ok(());
    };
    let scope = if repo.is_private {
        "private_and_internal"
    } else {
        "public"
    };
    let row:Option<Configuration>=sqlx::query_as("SELECT * FROM code_security_configurations WHERE org_id=$1 AND default_for_new_repos IN ('all',$2) ORDER BY id LIMIT 1").bind(org).bind(scope).fetch_optional(&mut *connection).await?;
    if let Some(config) = row {
        apply_repository(connection, repo.id, None, Some(&config)).await?;
    }
    Ok(())
}
